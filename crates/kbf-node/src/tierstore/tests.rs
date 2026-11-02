//! The `--tierstore-*` flags, the loopback rule, the start, and the plane they make: reads
//! through an in-process stand-in for the tierstore, with the frontend's plane underneath.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use bytes::Bytes;
use clap::Parser as _;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStoreError, PageSize,
};
use kbf_pack::{PackKeys, Packer, PackerConfig, WriterKind, blob_digest};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{NewPack, PackBlob};
use kbf_proto::reapi::Digest;
use kbf_state::ManualClock;
use kbf_transfer::CasError;
use kbf_transfer::cas::{digest_of, label};
use tokio::io::AsyncWriteExt as _;

use super::*;
use crate::tests::{scratch, tls_files};

/// The secret of the credentials files of these tests, which no message may carry.
const SECRET: &str = "s3cr3t-tier-key";

fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
    let base = [
        "kbf-daemon",
        "--server=https://front:7070",
        "--ca-cert=ca.pem",
        "--cert=c.pem",
        "--key=c.key",
        "--node-id=n1",
        "--driver=native",
    ];
    Cli::try_parse_from(base.iter().chain(extra))
}

/// The flags that turn the tierstore on at `endpoint`, over the key pair file `credentials`.
fn on(endpoint: &str, credentials: &Path) -> Vec<String> {
    vec![
        format!("--tierstore-endpoint={endpoint}"),
        "--tierstore-bucket=tierstore".to_owned(),
        format!("--tierstore-credentials-file={}", credentials.display()),
    ]
}

/// A key pair file in `dir` with this mode; its path.
fn keys(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    let text = format!(r#"{{"access_key_id": "AKIATIER", "secret_access_key": "{SECRET}"}}"#);
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// A daemon that moves blobs through the transfer engine (so its data plane is made at start),
/// with the TLS files of `dir`, and these more flags.
fn daemon(dir: &Path, more: &[String]) -> Cli {
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let mut argv = vec![
        "kbf-daemon".to_owned(),
        "--server=https://127.0.0.1:1".to_owned(),
        flag("ca-cert", &dir.join("ca.pem")),
        flag("cert", &dir.join("node.pem")),
        flag("key", &dir.join("node.key")),
        "--node-id=mac-1".to_owned(),
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        flag("data-dir", dir),
        flag("xcode-apps", &dir.join("Applications")),
    ];
    argv.extend_from_slice(more);
    Cli::try_parse_from(argv).expect("flags")
}

/// Asserts that the tierstore is off by default: no endpoint, no bucket, no credentials file,
/// no settings, and nothing for the other starts to refuse.
#[test]
fn the_tierstore_is_off_by_default() {
    let args = parse(&[]).expect("flags").tierstore;
    assert_eq!(args.tierstore_endpoint, None);
    assert_eq!(args.tierstore_bucket, None);
    assert_eq!(args.tierstore_credentials_file, None);
    assert_eq!(args.settings(), Ok(None));
    assert!(args.refuse("a test").is_ok());
}

/// Asserts that the three flags reach the settings as given.
#[test]
fn the_flags_reach_the_settings() {
    let flags = on("http://127.0.0.1:9300", Path::new("/keys.json"));
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let args = parse(&flags).expect("flags").tierstore;
    let want = Tierstore {
        endpoint: "http://127.0.0.1:9300".to_owned(),
        bucket: "tierstore".to_owned(),
        credentials: PathBuf::from("/keys.json"),
    };
    assert_eq!(args.settings(), Ok(Some(want)));
}

/// Asserts that the endpoint is refused without the bucket or the credentials file, that each
/// of those is refused without the endpoint, and that the endpoint is refused with
/// `--pack-read`, each time naming the flag.
/// Catches a daemon that starts half-configured, or reads packs from two stores.
#[test]
fn the_flags_come_together_and_never_with_pack_read() {
    let said = |flags: &[&str]| parse(flags).expect_err("refused").to_string();
    let endpoint = "--tierstore-endpoint=http://127.0.0.1:9300";
    let bucket = "--tierstore-bucket=tierstore";
    let file = "--tierstore-credentials-file=/keys.json";
    assert!(said(&[endpoint, bucket]).contains("--tierstore-credentials-file"));
    assert!(said(&[endpoint, file]).contains("--tierstore-bucket"));
    assert!(said(&[bucket]).contains("--tierstore-endpoint"));
    assert!(said(&[file]).contains("--tierstore-endpoint"));
    let both = said(&[
        endpoint,
        bucket,
        file,
        "--pack-read",
        "--pack-store-endpoint=http://127.0.0.1:9",
        "--pack-store-bucket=packs",
        "--pack-store-credentials-file=/keys.json",
    ]);
    assert!(both.contains("--pack-read"), "{both}");
    assert!(both.contains("--tierstore-endpoint"), "{both}");
}

/// Asserts the endpoints that are a tierstore on loopback: any `127.*` address, `::1`
/// and the IPv4-mapped form of a loopback address, with or without a port or a last `/`.
#[test]
fn loopback_endpoints_are_taken() {
    // The last loopback address of IPv4, spelled from its parts.
    let last = format!("http://{}:1", std::net::Ipv4Addr::new(127, 255, 255, 254));
    for endpoint in [
        "http://127.0.0.1:9300",
        "http://127.0.0.1:9300/",
        "http://127.0.0.1",
        &last,
        "http://[::1]:9300",
        "http://[::1]",
        "http://[::ffff:127.0.0.1]:9300",
    ] {
        assert_eq!(loopback(endpoint), Ok(()), "{endpoint}");
    }
}

/// Asserts that every endpoint that is not `http://` to a loopback IP address is refused, and
/// with what is wrong with it: another scheme or none, another address (the unspecified ones
/// included), a name (`localhost` included), a path, a query, a fragment, user info, an IPv6
/// address without brackets, and brackets that do not close or are followed by other than a
/// port. The whole message of one refusal is asserted, so that it names the flag, the endpoint
/// and the form it must take.
/// Catches a daemon that reads through a store off the host.
#[test]
fn other_endpoints_are_refused_with_what_is_wrong() {
    let cases = [
        ("https://127.0.0.1:9300", "not http://"),
        ("127.0.0.1:9300", "not http://"),
        ("", "not http://"),
        (
            "http://192.0.2.1:9300",
            "the address is not a loopback address",
        ),
        (
            "http://0.0.0.0:9300",
            "the address is not a loopback address",
        ),
        ("http://[::]:9300", "the address is not a loopback address"),
        (
            "http://[2001:db8::1]:9300",
            "the address is not a loopback address",
        ),
        (
            "http://[::ffff:192.0.2.1]:9300",
            "the address is not a loopback address",
        ),
        ("http://localhost:9300", "the host is not an IP address"),
        (
            "http://tierstore.example:9300",
            "the host is not an IP address",
        ),
        ("http://:9300", "the host is not an IP address"),
        (
            "http://127.0.0.1:9300/tierstore",
            "not a bare host and port",
        ),
        ("http://127.0.0.1:9300?x=1", "not a bare host and port"),
        ("http://127.0.0.1:9300#x", "not a bare host and port"),
        ("http://user@127.0.0.1:9300", "not a bare host and port"),
        ("http://[::1:9300", "not a bare host and port"),
        ("http://[::1]9300", "not a bare host and port"),
        ("http://::1:9300", "an IPv6 address goes in brackets"),
        (
            "http://[127.0.0.1]:9300",
            "an IPv4 address goes without brackets",
        ),
        ("HTTP://127.0.0.1:9300", "not http://"),
        ("http://127.0.0.1.:9300", "the host is not an IP address"),
        ("http://[::1%25lo]:9300", "the host is not an IP address"),
        (" http://127.0.0.1:9300", "not http://"),
    ];
    for (endpoint, why) in cases {
        let want = NotLoopback {
            endpoint: endpoint.to_owned(),
            why,
        };
        assert_eq!(loopback(endpoint), Err(want), "{endpoint}");
    }
    let said = loopback("http://192.0.2.1:9300")
        .expect_err("refused")
        .to_string();
    assert_eq!(
        said,
        "--tierstore-endpoint \"http://192.0.2.1:9300\" is not a tierstore on loopback: the \
         address is not a loopback address; give http://127.0.0.1:<port> or http://[::1]:<port>"
    );
}

/// Asserts that a daemon given an endpoint that is not on loopback is refused at start with
/// [`NotLoopback`], its message exactly, and no data plane made; the flags alone parse, so the
/// refusal is the start's.
#[test]
fn a_non_loopback_endpoint_is_refused_at_start_with_a_named_error() {
    let dir = scratch("tierstore-not-loopback");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", 0o600);
    let cli = daemon(&dir, &on("http://192.0.2.1:9300", &path));
    let refused = crate::start(&cli).err().expect("refused");
    let named = refused.downcast_ref::<NotLoopback>().expect("NotLoopback");
    let want = NotLoopback {
        endpoint: "http://192.0.2.1:9300".to_owned(),
        why: "the address is not a loopback address",
    };
    assert_eq!(named, &want);
    assert_eq!(refused.to_string(), want.to_string());
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// A writer the log lines of a test go into.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;

    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

/// What `start` makes of `cli`, said as the golden says it: each line the start logs about the
/// data plane, without time or target, and whether the daemon puts pack counts in its
/// heartbeats; or the error that refused the start.
fn started(cli: &Cli) -> Result<String, String> {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let counts = tracing::subscriber::with_default(subscriber, || {
        // A callsite another test reached first, with no subscriber, is cached as off.
        tracing::callsite::rebuild_interest_cache();
        let node = crate::start(cli).map_err(|e| e.to_string())?;
        Ok::<_, String>(node.daemon.pack_reads().is_some())
    })?;
    let held = sink
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let logged = String::from_utf8(held).expect("utf8");
    let mut said: String = logged
        .lines()
        .filter(|line| line.contains("data plane"))
        .map(|line| format!("{line}\n"))
        .collect();
    let counts = if counts { "carried" } else { "none" };
    said.push_str(&format!("pack counts in heartbeats: {counts}\n"));
    Ok(said)
}

/// Asserts that a daemon started without the flags says what `main`'s start said, byte for
/// byte: the golden holds the lines `main`'s start logged about its data plane (through `--cas`
/// until a map, then the storage nodes; no tierstore line) and that its heartbeats carry no pack
/// counts.
/// Catches a daemon without the flag that reads through a tierstore or counts pack reads.
#[test]
fn without_the_flags_the_daemon_starts_as_before_golden() {
    let dir = scratch("tierstore-golden");
    tls_files(&dir);
    let golden = include_str!("../../tests/golden/daemon-start-without-tierstore.txt");
    assert_eq!(started(&daemon(&dir, &[])).as_deref(), Ok(golden));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a daemon started with a loopback endpoint logs that its plane reads through the
/// tierstore, naming the endpoint and the bucket, and puts the pack counts in its heartbeats;
/// nothing is asked of the tierstore at start.
#[test]
fn a_daemon_starts_reading_through_the_tierstore_and_carries_the_counts() {
    let dir = scratch("tierstore-start");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", 0o600);
    let said = started(&daemon(&dir, &on("http://127.0.0.1:9", &path))).expect("starts");
    assert_eq!(
        said,
        " INFO data plane: through --cas until the frontend sends a storage map, then to the \
         storage nodes it names\n INFO data plane: blobs are read through the tierstore on \
         loopback first (endpoint http://127.0.0.1:9, bucket tierstore), and from --cas or the \
         storage nodes when it does not serve them; outputs go where they went\npack counts in \
         heartbeats: carried\n"
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that what the S3 client cannot be built from stops the start, naming the flag and
/// never the secret: a credentials file others may read (named by its file), one that is not
/// there, a bucket the client refuses, and a loopback endpoint with a port that is not one.
#[test]
fn what_the_client_cannot_use_stops_the_start_with_the_flag_named() {
    let dir = scratch("tierstore-refused");
    tls_files(&dir);
    let good = keys(&dir, "keys.json", 0o600);
    let open = keys(&dir, "open.json", 0o644);
    let refused = |flags: Vec<String>| started(&daemon(&dir, &flags)).expect_err("refused");

    let mut upper = on("http://127.0.0.1:9", &good);
    upper[1] = "--tierstore-bucket=UPPER".to_owned();
    let cases = [
        (
            on("http://127.0.0.1:9", &open),
            "--tierstore-credentials-file ",
        ),
        (
            on("http://127.0.0.1:9", &dir.join("none.json")),
            "--tierstore-credentials-file ",
        ),
        (upper, "--tierstore-bucket: "),
        (on("http://127.0.0.1:port", &good), "--tierstore-endpoint: "),
    ];
    let mut said = Vec::new();
    for (flags, flag) in cases {
        let refusal = refused(flags);
        assert!(refusal.starts_with(flag), "{refusal}");
        assert!(!refusal.contains(SECRET), "{refusal}");
        said.push(refusal);
    }
    assert!(said[0].contains("open.json"), "{}", said[0]);
    assert!(said[1].contains("none.json"), "{}", said[1]);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that the daemon that reads blobs in-process (`start_with`: `kbf all-in-one`, and `kbf
/// storage-and-daemon` without a `--frontend`) refuses the flags at its start, naming the flag.
#[test]
fn the_in_process_start_refuses_the_tierstore() {
    let dir = scratch("tierstore-in-process");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", 0o600);
    let cli = daemon(&dir, &on("http://127.0.0.1:9", &path));
    let refused = crate::start_with(&cli, crate::remote)
        .err()
        .expect("refused");
    assert_eq!(
        refused.to_string(),
        "--tierstore-endpoint: only `kbf daemon` reads through a local tierstore, not `kbf \
         all-in-one` or `kbf storage-and-daemon` without `--frontend`"
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// The frontend's plane, as the daemon reads it without the flag: blobs in memory, and the calls
/// made on it.
#[derive(Clone, Default)]
struct Front {
    blobs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl Front {
    fn note(&self, call: &'static str) {
        let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        calls.push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        let calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        calls.clone()
    }

    fn insert(&self, bytes: &[u8]) -> Digest {
        let digest = digest_of(bytes);
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.insert(label(&digest), bytes.to_vec());
        digest
    }

    fn blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let held = blobs.get(&label(digest)).cloned();
        held.ok_or_else(|| CasError::Missing(label(digest)))
    }
}

impl Cas for Front {
    type Lease = Front;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Front {
        self.note("lease");
        Front::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.note("get");
        self.blob(digest)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        self.note("read_to");
        let bytes = self.blob(digest)?;
        file.write_all(&bytes).await.expect("write");
        file.flush().await.expect("flush");
        Ok(())
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.note("put");
        Ok(self.insert(&bytes))
    }
}

/// The tierstore, in process: a memory object store that counts the ranged reads made of it
/// and, while `down`, fails each one as a connection that was refused.
struct Tier {
    store: MemoryStore,
    reads: AtomicUsize,
    down: AtomicBool,
}

impl Tier {
    fn new() -> Self {
        Self {
            // Create-only, so that the test's packer, the farm's writer of packs here, takes it.
            store: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            reads: AtomicUsize::new(0),
            down: AtomicBool::new(false),
        }
    }

    fn store(&self) -> &MemoryStore {
        &self.store
    }
}

impl ObjectStore for Tier {
    fn capabilities(&self) -> Capabilities {
        self.store().capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.store().put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
            return Err(ObjectStoreError::Transport(Arc::new(refused)));
        }
        self.store().get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.store().delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.store().list(prefix, after, max_keys).await
    }
}

/// Where the pack of a blob that is recorded in the index without being stored is.
const FORGED: &str = "cas/d/node1/boot1/00000000000000ff";

/// The frontend's plane, the tierstore, the frontend's pack index, a packer over the last two,
/// and the plane [`over`] makes of them.
struct Rig {
    front: Front,
    tier: Arc<Tier>,
    index: Arc<MemoryPackIndex>,
    packer: Packer<Tier>,
    counter: PackCounter,
    cas: Arc<PackedCas<Front>>,
}

impl Rig {
    fn new() -> Self {
        let tier = Arc::new(Tier::new());
        let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let shared: Arc<dyn PackIndex> = index.clone();
        let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
        let packer = Packer::new(
            Arc::clone(&tier),
            Arc::clone(&shared),
            keys,
            PackerConfig::default(),
        );
        let front = Front::default();
        let counter = PackCounter::default();
        let cas = over(Arc::new(front.clone()), Arc::clone(&tier), shared, &counter);
        Self {
            front,
            tier,
            index,
            packer: packer.expect("packer"),
            counter,
            cas,
        }
    }

    /// Packs `bytes` into the tierstore, as a writer of the farm's packs would, and records the
    /// pack in the index; their digest.
    async fn pack(&self, bytes: &'static [u8]) -> Digest {
        let blob = kbf_pack::Blob {
            digest: blob_digest(bytes),
            bytes: Bytes::from_static(bytes),
        };
        self.packer.pack(vec![blob]).await.expect("packed");
        digest_of(bytes)
    }

    /// Records in the index a pack that holds `true_bytes` at its start, and stores `stored` as
    /// that pack's bytes in the tierstore if given (`None`: the tierstore answers 404 for it).
    /// The digest of `true_bytes`.
    async fn forge(&self, true_bytes: &'static [u8], stored: Option<&'static [u8]>) -> Digest {
        let pack = NewPack {
            key: FORGED.to_owned(),
            writer: "node1".to_owned(),
            size: true_bytes.len() as u64,
            blobs: vec![PackBlob {
                digest: blob_digest(true_bytes),
                offset: 0,
            }],
        };
        self.index.record_pack(&pack).expect("recorded");
        if let Some(stored) = stored {
            let key = ObjectKey::new(FORGED).expect("a key");
            let put = self
                .tier
                .put_new(&key, Bytes::from_static(stored), None)
                .await;
            put.expect("stored");
        }
        digest_of(true_bytes)
    }

    fn reads(&self) -> usize {
        self.tier.reads.load(Ordering::SeqCst)
    }
}

/// `digest` read with `get`, and with `read_to` into a fresh file of `dir`: the two answers,
/// the second as the bytes the file holds after it.
async fn both(cas: &PackedCas<Front>, dir: &Path, digest: &Digest) -> (Vec<u8>, Vec<u8>) {
    let got = cas.get(digest).await.expect("get");
    let path = dir.join(label(digest).replace('/', "-"));
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    cas.read_to(digest, &mut file).await.expect("read_to");
    (got, std::fs::read(&path).expect("read back"))
}

/// Asserts that a blob the tierstore holds is served from it to `get` and to `read_to`, whole,
/// and that the frontend's CAS plane is never called: the bytes come from the tierstore only
/// (the blob's place comes from the frontend's pack index, here in memory).
/// Catches a read that fetches the bytes from the frontend though the tierstore has the blob.
#[tokio::test]
async fn a_hit_is_served_from_the_tierstore_and_never_read_from_the_frontends_cas() {
    let dir = scratch("tierstore-hit");
    let rig = Rig::new();
    let digest = rig.pack(b"in the tierstore").await;
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"in the tierstore");
    assert_eq!(written, b"in the tierstore");
    assert_eq!(rig.front.calls(), Vec::<&str>::new());
    assert_eq!(rig.reads(), 2);
    let stats = rig.counter.stats();
    assert_eq!((stats.served, stats.served_bytes), (2, 32));
    assert_eq!(
        stats.elsewhere + stats.unasked + stats.lost + stats.damaged,
        0
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a blob the frontend's index places in no pack is read from the frontend, with
/// the frontend's bytes, the tierstore not asked for it, and counted as read elsewhere.
#[tokio::test]
async fn a_blob_in_no_pack_is_read_from_the_frontend() {
    let dir = scratch("tierstore-unplaced");
    let rig = Rig::new();
    let digest = rig.front.insert(b"only at the frontend");
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(
        (got.as_slice(), written.as_slice()),
        (&b"only at the frontend"[..], &b"only at the frontend"[..])
    );
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.reads(), 0);
    assert_eq!(rig.counter.stats().elsewhere, 2);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a pack the tierstore answers 404 for (a miss its upstream does not hold either)
/// costs a read from the frontend and never a failed one: both reads answer the frontend's
/// bytes, and are counted as lost.
/// Catches a tierstore miss that fails a lease's input fetch.
#[tokio::test]
async fn a_miss_in_the_tierstore_falls_back_to_the_frontend_and_the_read_succeeds() {
    let dir = scratch("tierstore-miss");
    let rig = Rig::new();
    let digest = rig.forge(b"not in the tierstore", None).await;
    rig.front.insert(b"not in the tierstore");
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"not in the tierstore");
    assert_eq!(written, b"not in the tierstore");
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.reads(), 2);
    let stats = rig.counter.stats();
    assert_eq!((stats.lost, stats.served), (2, 0));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that bytes from the tierstore that are not the digest are never served: both reads
/// answer the frontend's true bytes, and are counted as damaged.
/// Catches a damaged cached copy that reaches a lease, or that fails its read.
#[tokio::test]
async fn damaged_bytes_from_the_tierstore_fall_back_to_the_frontend() {
    let dir = scratch("tierstore-damaged");
    let rig = Rig::new();
    let digest = rig.forge(b"true bytes", Some(b"xxxx bytes")).await;
    rig.front.insert(b"true bytes");
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"true bytes");
    assert_eq!(written, b"true bytes");
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.counter.stats().damaged, 2);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a tierstore that is down (each read refused) costs reads from the frontend and
/// never a failed one, counted as unasked; and that once it is up again, with no restart, the
/// same blob is served from it again and the frontend is not asked.
/// Catches a dead tierstore that fails builds, or a daemon that stops using one that came back.
#[tokio::test]
async fn a_tierstore_that_is_down_is_read_around_and_used_again_once_up() {
    let dir = scratch("tierstore-down");
    let rig = Rig::new();
    let digest = rig.pack(b"packed and at the frontend").await;
    rig.front.insert(b"packed and at the frontend");
    rig.tier.down.store(true, Ordering::SeqCst);
    let (got, written) = both(&rig.cas, &dir, &digest).await;
    assert_eq!(got, b"packed and at the frontend");
    assert_eq!(written, b"packed and at the frontend");
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.counter.stats().unasked, 2);

    rig.tier.down.store(false, Ordering::SeqCst);
    assert_eq!(
        rig.cas.get(&digest).await,
        Ok(b"packed and at the frontend".to_vec())
    );
    assert_eq!(rig.front.calls(), ["get", "read_to"]);
    assert_eq!(rig.counter.stats().served, 1);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a blob over 64 MiB, which a read from a pack would hold whole, is read from the
/// frontend without asking the tierstore, and that a blob in neither is the frontend's
/// `Missing`, as without the flag.
#[tokio::test]
async fn a_large_blob_and_a_blob_nowhere_are_the_frontends() {
    let rig = Rig::new();
    let large = Digest {
        hash: "ab".repeat(32),
        size_bytes: (64 << 20) + 1,
    };
    assert_eq!(
        rig.cas.get(&large).await,
        Err(CasError::Missing(label(&large)))
    );
    let nowhere = digest_of(b"nowhere");
    assert_eq!(
        rig.cas.get(&nowhere).await,
        Err(CasError::Missing(label(&nowhere)))
    );
    assert_eq!(rig.front.calls(), ["get", "get"]);
    assert_eq!(rig.reads(), 0);
}

/// Asserts that a lease's plane reads through the tierstore as its plane does, and falls back
/// as it does: a hit is not read from the frontend, a miss is.
#[tokio::test]
async fn a_lease_reads_through_the_tierstore_and_falls_back() {
    let rig = Rig::new();
    let hit = rig.pack(b"leased hit").await;
    let miss = rig.forge(b"leased miss", None).await;
    rig.front.insert(b"leased miss");
    let lease = rig.cas.lease(&kbf_types::LeaseId::new(1, 2));
    assert_eq!(lease.get(&hit).await, Ok(b"leased hit".to_vec()));
    assert_eq!(lease.get(&miss).await, Ok(b"leased miss".to_vec()));
    assert_eq!(rig.front.calls(), ["lease", "get"]);
}
