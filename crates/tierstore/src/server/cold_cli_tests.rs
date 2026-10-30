//! The start of a pass-through front: what a server started from a file with `[cold_store]` alone
//! serves, and the refusals of its start.

use std::ffi::OsString;
use std::fs::Permissions;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError,
    PageSize,
};
use kbf_testkit::dir::TestDir;
use tokio::sync::oneshot;

use super::cli::{Error, exit_status};
use super::cold_cli::{run_cold, serve_cold};
use super::config::{PerRun, settings_from_text};
use super::config_cold::ColdSettings;
use super::launch::{Refused, resolve};
use super::presign::{PresignRequest, presign_put};
use super::{AccessKey, Drained};
use crate::selftest_tests::{Flaw, Flawed};
use crate::{Backend, Clock, MemoryBackend, SystemClock};

const BUCKET: &str = "tier-bucket";
const KEY_ID: &str = "AKIDCOLD";
const SECRET: &str = "cold-secret";
const READ_ID: &str = "AKIDREAD";
const READ_SECRET: &str = "read-secret";
const GC_ID: &str = "gc-key";
const GC_SECRET: &str = "gc-secret-0123456789";
const SCOPED_ID: &str = "scoped-key";
const SCOPED_SECRET: &str = "scoped-secret-0123456789";
const PRESIGN_ID: &str = "presign-key";
const PRESIGN_SECRET: &str = "presign-secret-0123456789";

/// The knobs of a run whose drain does not wait.
const QUICK: PerRun = PerRun {
    drain_lb_wait: Duration::from_millis(50),
    drain_deadline: Duration::from_secs(60),
};

/// A scratch directory with the key file of every role in it.
struct Node {
    dir: TestDir,
}

impl Node {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let node = Self { dir };
        node.secret("key", &format!("{KEY_ID}\n{SECRET}\n"));
        node.secret("read", &format!("{READ_ID}\n{READ_SECRET}\n"));
        node.secret(
            "gc",
            &format!(
                "{{\"access_key_id\": \"{GC_ID}\", \"secret_access_key\": \"{GC_SECRET}\", \"prefix\": \"gc/\"}}"
            ),
        );
        node.secret(
            "scoped",
            &format!(
                "{{\"access_key_id\": \"{SCOPED_ID}\", \"secret_access_key\": \"{SCOPED_SECRET}\", \
                 \"read_prefixes\": [\"r/\"], \"write_prefixes\": [\"w/\"], \"delete_prefixes\": [\"d/\"]}}"
            ),
        );
        node.secret(
            "presign",
            &format!(
                "{{\"access_key_id\": \"{PRESIGN_ID}\", \"secret_access_key\": \"{PRESIGN_SECRET}\", \
                 \"ceiling\": {{\"prefix\": \"p/\", \"max_expires_secs\": 600, \"max_object_bytes\": 1048576}}}}"
            ),
        );
        node
    }

    /// A file called `name` that only its owner can read.
    fn secret(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn file(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// The text of a pass-through file with this extra text in `[server]` and `cold` in
    /// `[cold_store]`, and a key file for each role.
    fn text(&self, server: &str, cold: &str) -> String {
        format!(
            r#"
[server]
listen              = "127.0.0.1:0"
proxy_id            = "pass-1"
bucket              = "{BUCKET}"
access_key_file     = "{key}"
read_only_key_files = ["{read}"]
gc_key_files        = ["{gc}"]
scoped_key_files    = ["{scoped}"]
presign_key_files   = ["{presign}"]
{server}

[cold_store]
kind = "memory"
{cold}
"#,
            key = self.file("key"),
            read = self.file("read"),
            gc = self.file("gc"),
            scoped = self.file("scoped"),
            presign = self.file("presign"),
        )
    }

    fn settings(&self, server: &str, cold: &str) -> ColdSettings {
        let text = self.text(server, cold);
        settings_from_text(&text, QUICK)
            .unwrap()
            .into_cold()
            .unwrap()
    }
}

/// A client of kbf for the server at `addr`, signing with this key.
fn client_as(addr: SocketAddr, key: (&str, &str)) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: BUCKET.to_owned(),
        credentials: Credentials::new(key.0, key.1),
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

fn client(addr: SocketAddr) -> S3Store {
    client_as(addr, (KEY_ID, SECRET))
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// The text an unsigned GET of `path` is answered with, and its status.
async fn probe(addr: SocketAddr, path: &str) -> (u16, String) {
    let reply = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
    (reply.status().as_u16(), reply.text().await.unwrap())
}

/// The names of the objects under `prefix`, as the client lists them.
async fn names(store: &S3Store, prefix: &str) -> Vec<String> {
    let under = KeyPrefix::new(prefix).unwrap();
    let page = store.list(&under, None, PageSize::MAX).await.unwrap();
    page.objects
        .into_iter()
        .map(|info| info.key.as_str().to_owned())
        .collect()
}

/// An upload of `key` that is open, with the parts `parts` sent to it: the id and the numbers and
/// tags to complete it with.
async fn open_upload(
    store: &S3Store,
    key: &ObjectKey,
    parts: &[&'static [u8]],
) -> (String, Vec<(u32, String)>) {
    let id = store.create_multipart(key).await.unwrap();
    let mut done = Vec::new();
    for (at, &part) in parts.iter().enumerate() {
        let number = u32::try_from(at).unwrap() + 1;
        let body: BodyStream =
            futures::stream::once(async move { Ok(Bytes::from_static(part)) }).boxed();
        let len = part.len() as u64;
        let tag = store.upload_part_stream(key, &id, number, len, body).await;
        done.push((number, tag.unwrap()));
    }
    (id, done)
}

/// Whether `result` is the refusal 403 `AccessDenied`.
fn denied<T: std::fmt::Debug>(result: &Result<T, ObjectStoreError>) -> bool {
    matches!(
        result,
        Err(ObjectStoreError::Service { status: 403, code, .. }) if code == "AccessDenied"
    )
}

/// Runs `serve_cold` over this cold store, calls `work` with the address, then stops it.
async fn serving<F, Fut>(
    settings: &ColdSettings,
    cold: Arc<dyn Backend>,
    work: F,
) -> Result<Drained, Error>
where
    F: FnOnce(SocketAddr) -> Fut,
    Fut: Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = serve_cold(
        settings,
        cold,
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

/// Asserts that a server started from a file with `[cold_store]` alone serves the session a
/// storage node serves, straight from the cold store: a create-only put, a whole and a ranged
/// get, a head, a listing, a delete, a multipart upload (listed while open, then completed), the
/// probes (`/readyz` ready, `/usagez` not watched, `/metricz` with no cache); that what it stores
/// is in the cold store and nothing else is (no journal, no lease, no object of the proxy's);
/// and that it drains with nothing cut.
#[tokio::test]
async fn a_cold_store_alone_serves_the_session_of_a_storage_node() {
    let node = Node::new("tscold-cli-session");
    let settings = node.settings("", "");
    let memory = Arc::new(MemoryBackend::new());
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        let a = key("cas/a");
        store
            .put_new(&a, Bytes::from_static(b"hello"), None)
            .await
            .unwrap();
        let all = ByteRange::new(0, 5).unwrap();
        assert_eq!(store.get_range(&a, all).await.unwrap(), "hello");
        let middle = ByteRange::new(1, 3).unwrap();
        assert_eq!(store.get_range(&a, middle).await.unwrap(), "ell");
        assert_eq!(store.len(&a).await.unwrap(), Some(5));
        assert_eq!(names(&store, "cas/").await, ["cas/a"]);
        store.delete(&a).await.unwrap();
        assert_eq!(store.len(&a).await.unwrap(), None);

        let m = key("cas/m");
        let (id, parts) = open_upload(&store, &m, &[b"abc", b"def"]).await;
        let under = KeyPrefix::new("cas/").unwrap();
        let open = store
            .list_multipart_uploads(&under, None, PageSize::MAX)
            .await
            .unwrap();
        assert_eq!(open.uploads.len(), 1);
        assert_eq!(open.uploads[0].upload_id, id);
        store.complete_multipart(&m, &id, &parts).await.unwrap();
        let whole = ByteRange::new(0, 6).unwrap();
        assert_eq!(store.get_range(&m, whole).await.unwrap(), "abcdef");

        assert_eq!(probe(addr, "/readyz").await, (200, "ready".to_owned()));
        let (status, text) = probe(addr, "/usagez").await;
        assert_eq!((status, text.as_str()), (200, "usage not watched"));
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        assert!(text.contains("no local cache"), "{text}");
    };
    let drained = serving(&settings, cold, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    let kept = memory.list("").await.unwrap();
    let kept: Vec<&str> = kept.iter().map(|listed| listed.key.as_str()).collect();
    assert_eq!(kept, ["cas/m"]);
    assert_eq!(memory.get("cas/m", None).await.unwrap(), "abcdef");
}

/// Asserts that create-only holds at the cold store for a put and for the completion of an upload:
/// a second put of a key that exists is refused and the bytes are the first's; the completion of
/// an upload of a key that exists is refused the same way, the bytes are the first's, and the
/// upload is still open and can be aborted. Catches a front that replaces an object on a second
/// put or on a completion.
#[tokio::test]
async fn create_only_holds_for_a_put_and_for_a_completion() {
    let node = Node::new("tscold-cli-create-only");
    let settings = node.settings("", "");
    let memory = Arc::new(MemoryBackend::new());
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        let x = key("cas/x");
        store
            .put_new(&x, Bytes::from_static(b"first"), None)
            .await
            .unwrap();
        let again = store.put_new(&x, Bytes::from_static(b"second"), None).await;
        assert!(
            matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
            "{again:?}"
        );

        let (id, parts) = open_upload(&store, &x, &[b"third"]).await;
        let done = store.complete_multipart(&x, &id, &parts).await;
        assert!(
            matches!(done, Err(ObjectStoreError::AlreadyExists(_))),
            "{done:?}"
        );
        let all = ByteRange::new(0, 5).unwrap();
        assert_eq!(store.get_range(&x, all).await.unwrap(), "first");
        let under = KeyPrefix::new("cas/").unwrap();
        let open = store
            .list_multipart_uploads(&under, None, PageSize::MAX)
            .await
            .unwrap();
        assert_eq!(open.uploads.len(), 1);
        store.abort_multipart(&x, &id).await.unwrap();
        let open = store
            .list_multipart_uploads(&under, None, PageSize::MAX)
            .await
            .unwrap();
        assert!(open.uploads.is_empty());
    };
    serving(&settings, cold, work).await.unwrap();
    assert_eq!(memory.get("cas/x", None).await.unwrap(), "first");
}

/// Asserts that each role of a key is held to what it may do by a front over the cold store alone:
/// a read-only key reads and lists and is refused a put and a delete; a GC key lists, heads and
/// deletes under its prefix and is refused a get and a put; a scoped key reads its read prefix,
/// creates once under its write prefix (create-only), deletes under its delete prefix, and is
/// refused the rest; a presigning key is refused a header-signed call. Each refusal is 403
/// `AccessDenied` and changes nothing at the cold store.
#[tokio::test]
async fn each_key_role_is_held_to_what_it_may_do() {
    let node = Node::new("tscold-cli-roles");
    let settings = node.settings("", "");
    let memory = Arc::new(MemoryBackend::new());
    for (name, body) in [
        ("cas/o", "one"),
        ("gc/o", "two"),
        ("r/o", "three"),
        ("d/o", "four"),
    ] {
        memory.put(name, Bytes::from(body)).await.unwrap();
    }
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let all = ByteRange::new(0, 5).unwrap();
        let read = client_as(addr, (READ_ID, READ_SECRET));
        assert_eq!(read.get_range(&key("cas/o"), all).await.unwrap(), "one");
        assert_eq!(names(&read, "gc/").await, ["gc/o"]);
        let put = read
            .put_new(&key("cas/n"), Bytes::from_static(b"x"), None)
            .await;
        assert!(denied(&put), "{put:?}");
        assert!(denied(&read.delete(&key("cas/o")).await));

        let gc = client_as(addr, (GC_ID, GC_SECRET));
        assert_eq!(names(&gc, "gc/").await, ["gc/o"]);
        assert_eq!(gc.len(&key("gc/o")).await.unwrap(), Some(3));
        assert!(denied(&gc.get_range(&key("gc/o"), all).await));
        let put = gc
            .put_new(&key("gc/n"), Bytes::from_static(b"x"), None)
            .await;
        assert!(denied(&put), "{put:?}");
        assert!(denied(&gc.delete(&key("cas/o")).await));
        gc.delete(&key("gc/o")).await.unwrap();

        let scoped = client_as(addr, (SCOPED_ID, SCOPED_SECRET));
        assert_eq!(scoped.get_range(&key("r/o"), all).await.unwrap(), "three");
        assert!(denied(&scoped.get_range(&key("cas/o"), all).await));
        let made = key("w/n");
        scoped
            .put_new(&made, Bytes::from_static(b"new"), None)
            .await
            .unwrap();
        let again = scoped
            .put_new(&made, Bytes::from_static(b"newer"), None)
            .await;
        assert!(
            matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
            "{again:?}"
        );
        assert!(denied(&scoped.get_range(&made, all).await));
        assert!(denied(&scoped.delete(&made).await));
        scoped.delete(&key("d/o")).await.unwrap();

        let signer = client_as(addr, (PRESIGN_ID, PRESIGN_SECRET));
        let put = signer
            .put_new(&key("p/n"), Bytes::from_static(b"x"), None)
            .await;
        assert!(denied(&put), "{put:?}");
    };
    serving(&settings, cold, work).await.unwrap();
    let mut kept: Vec<String> = memory
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|listed| listed.key)
        .collect();
    kept.sort();
    assert_eq!(kept, ["cas/o", "r/o", "w/n"]);
    assert_eq!(memory.get("w/n", None).await.unwrap(), "new");
}

/// Asserts that a presigned put is taken by a front over the cold store alone, create-only: a URL
/// of the presigning key puts the object at the cold store, the same URL used again is 412 and
/// the bytes are the first's, and a URL for a key outside the key's prefix is 403.
#[tokio::test]
async fn a_presigned_put_goes_to_the_cold_store_create_only() {
    let node = Node::new("tscold-cli-presign");
    let settings = node.settings("", "");
    let memory = Arc::new(MemoryBackend::new());
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let signer = AccessKey::new(PRESIGN_ID, PRESIGN_SECRET);
        let endpoint = format!("http://{addr}");
        let url = |name: &str| {
            let request = PresignRequest {
                endpoint: &endpoint,
                region: "us-east-1",
                bucket: BUCKET,
                key: name,
                expires_secs: 300,
                now: SystemClock.now(),
            };
            presign_put(&signer, &request).unwrap()
        };
        let http = reqwest::Client::new();
        let put = |url: String, body: &'static str| http.put(url).body(body).send();
        let first = put(url("p/obj"), "payload").await.unwrap();
        assert_eq!(first.status().as_u16(), 200);
        let second = put(url("p/obj"), "other").await.unwrap();
        assert_eq!(second.status().as_u16(), 412);
        let outside = put(url("q/obj"), "payload").await.unwrap();
        assert_eq!(outside.status().as_u16(), 403);
    };
    serving(&settings, cold, work).await.unwrap();
    assert_eq!(memory.get("p/obj", None).await.unwrap(), "payload");
    assert!(memory.head("q/obj").await.is_err());
}

/// Asserts that a cold store that does not keep create-only is not served over, with exit status
/// 3 and the store named: one whose create-only put overwrites, and one whose completion of an
/// upload overwrites (the self-test of the start asks both of a cold store that is alone, and
/// nothing can be skipped). The address is never bound.
#[tokio::test]
async fn a_cold_store_that_does_not_keep_create_only_is_refused_with_status_3() {
    let node = Node::new("tscold-cli-flawed");
    let settings = node.settings("", "");
    for flaw in [Flaw::OverwritesOnCreate, Flaw::CompletionOverwrites] {
        let cold: Arc<dyn Backend> = Arc::new(Flawed::new(flaw));
        let nothing = |_addr: SocketAddr| async { panic!("served over a flawed store") };
        let why = serving(&settings, cold, nothing).await.unwrap_err();
        assert!(why.to_string().contains("cold tier memory"), "{why}");
        assert_eq!(exit_status(&why), 3, "{why}");
    }
}

/// Asserts that the file of a cold store that cannot claim the conditional completion is refused
/// when the command line is resolved (exit status 2 in the binary), naming the key: the only way
/// a file could say the store does not keep create-only on a completion is `skip_checks`.
#[test]
fn a_file_that_skips_a_check_is_refused_as_a_config() {
    let node = Node::new("tscold-cli-skip");
    let skip =
        "skip_checks = [\"multipart-conditional=store-ignores-if-none-match-on-completion\"]";
    let path = node.secret("skip.toml", &node.text("", skip));
    let args: Vec<OsString> = vec!["tierstore".into(), "--config".into(), path.into_os_string()];
    match resolve(args) {
        Err(Refused::Config(why)) => {
            let why = why.to_string();
            assert!(why.contains("cold_store.skip_checks"), "{why}");
            assert!(why.contains("skip.toml"), "{why}");
        }
        other => panic!("not refused as a config: {other:?}"),
    }
}

/// Asserts the refusals of a start, each with the exit status of its reason: a request timeout of
/// zero, a bucket that is a probe path, an address that is taken, a key file for the cold store
/// that is not there (1 each), and a cold store that does not answer for the whole of
/// `backend_check_wait` (1, a restart may mend it).
#[tokio::test]
async fn a_pass_through_front_that_cannot_start_exits_with_the_status_of_the_reason() {
    let node = Node::new("tscold-cli-refused");
    let cold: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let nothing = |_addr: SocketAddr| async {};

    let zero = node.settings("s3_request_timeout = \"0s\"", "");
    let why = run_cold(&zero, std::future::ready(()), |_| {})
        .await
        .unwrap_err();
    assert!(why.to_string().contains("--s3-request-timeout"), "{why}");
    assert_eq!(exit_status(&why), 1);

    for name in ["healthz", "readyz", "usagez", "metricz"] {
        let text = node.text("", "").replace(BUCKET, name);
        let renamed = settings_from_text(&text, QUICK)
            .unwrap()
            .into_cold()
            .unwrap();
        let why = serving(&renamed, cold.clone(), nothing).await.unwrap_err();
        assert!(why.to_string().contains("probe path"), "{name}: {why}");
        assert_eq!(exit_status(&why), 1);
    }

    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap().to_string();
    let text = node.text("", "").replace("127.0.0.1:0", &address);
    let busy = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cold()
        .unwrap();
    let why = serving(&busy, cold.clone(), nothing).await.unwrap_err();
    assert!(
        why.to_string().starts_with(&format!("binding {address}")),
        "{why}"
    );
    assert_eq!(exit_status(&why), 1);

    let missing = node.file("no-such-credentials");
    let s3 = format!(
        "kind = \"s3\"\nbucket = \"b\"\nendpoint = \"http://127.0.0.1:1\"\ncredentials_file = \"{missing}\""
    );
    let text = node.text("", "").replace("kind = \"memory\"", &s3);
    let remote = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cold()
        .unwrap();
    let why = run_cold(&remote, std::future::ready(()), |_| {})
        .await
        .unwrap_err();
    assert!(why.to_string().contains("credentials"), "{why}");
    assert_eq!(exit_status(&why), 1);

    let patient = node.settings("backend_check_wait = \"0s\"", "");
    let down: Arc<dyn Backend> = Arc::new(Flawed::new(Flaw::Down));
    let why = serving(&patient, down, nothing).await.unwrap_err();
    assert!(why.to_string().contains("cold tier memory"), "{why}");
    assert_eq!(exit_status(&why), 1);
}

/// Asserts that `run_cold` makes the cold store the file names (a memory tier here, empty) and
/// serves it: a key that is not there is not found, and a put is taken.
#[tokio::test]
async fn run_cold_opens_the_cold_store_the_file_names() {
    let node = Node::new("tscold-cli-run");
    let settings = node.settings("", "");
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = run_cold(
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
        let none = key("cas/none");
        assert_eq!(store.len(&none).await.unwrap(), None);
        store
            .put_new(&none, Bytes::from_static(b"x"), None)
            .await
            .unwrap();
        assert_eq!(store.len(&none).await.unwrap(), Some(1));
        stop.send(()).unwrap();
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
}
