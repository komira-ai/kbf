//! The `--tierstore-*` flags, the loopback rule and the starts. The plane they make is
//! `plane.rs`'s.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use clap::Parser as _;

use super::*;
use crate::tests::{scratch, tls_files};

mod plane;

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
    assert_eq!(args.settings().expect("no settings"), None);
    assert!(args.refuse("a test").is_ok());
}

/// Asserts that the three flags reach the settings as given.
#[test]
fn the_flags_reach_the_settings() {
    let flags = on("http://127.0.0.1:9300", Path::new("/keys.json"));
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let args = parse(&flags).expect("flags").tierstore;
    let want = Tierstore {
        endpoint: Endpoint {
            given: "http://127.0.0.1:9300".to_owned(),
            addr: SocketAddr::from(([127, 0, 0, 1], 9300)),
            authority: "127.0.0.1:9300".to_owned(),
        },
        bucket: "tierstore".to_owned(),
        credentials: PathBuf::from("/keys.json"),
    };
    assert_eq!(args.settings().expect("settings"), Some(want));
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

/// Asserts the endpoints that are a tierstore on loopback, and the address and authority each
/// is reached at: any `127.*` address, `::1` and the IPv4-mapped form of a loopback address, with
/// or without a port (80 when there is none), with or without a last `/`, ports 1 and 65535.
#[test]
fn loopback_endpoints_are_taken() {
    // The last loopback address of IPv4, spelled from its parts.
    let last_ip = std::net::Ipv4Addr::new(127, 255, 255, 254);
    let last = format!("http://{last_ip}:1");
    let v6 = |port: u16| SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    let mapped: IpAddr = "::ffff:127.0.0.1".parse().expect("an address");
    let cases = [
        (
            "http://127.0.0.1:9300",
            SocketAddr::from(([127, 0, 0, 1], 9300)),
            "127.0.0.1:9300",
        ),
        (
            "http://127.0.0.1:9300/",
            SocketAddr::from(([127, 0, 0, 1], 9300)),
            "127.0.0.1:9300",
        ),
        (
            "http://127.0.0.1",
            SocketAddr::from(([127, 0, 0, 1], 80)),
            "127.0.0.1",
        ),
        (
            "http://127.0.0.1:65535",
            SocketAddr::from(([127, 0, 0, 1], 65535)),
            "127.0.0.1:65535",
        ),
        (&last, SocketAddr::from((last_ip, 1)), &last[7..]),
        ("http://[::1]:9300", v6(9300), "[::1]:9300"),
        ("http://[::1]", v6(80), "[::1]"),
        (
            "http://[::ffff:127.0.0.1]:9300",
            SocketAddr::new(mapped, 9300),
            "[::ffff:127.0.0.1]:9300",
        ),
    ];
    for (endpoint, addr, authority) in cases {
        let want = Endpoint {
            given: endpoint.to_owned(),
            addr,
            authority: authority.to_owned(),
        };
        assert_eq!(loopback(endpoint), Ok(want), "{endpoint}");
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
        (
            "http://127.0.0.1:port",
            "the port is not a number from 1 to 65535",
        ),
        (
            "http://127.0.0.1:0",
            "the port is not a number from 1 to 65535",
        ),
        (
            "http://127.0.0.1:65536",
            "the port is not a number from 1 to 65535",
        ),
        (
            "http://127.0.0.1:",
            "the port is not a number from 1 to 65535",
        ),
        ("http://[::1]:", "the port is not a number from 1 to 65535"),
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
    let named = refused.downcast_ref::<TierstoreError>();
    let Some(TierstoreError::NotLoopback(named)) = named else {
        panic!("not NotLoopback: {refused:?}");
    };
    let want = NotLoopback {
        endpoint: "http://192.0.2.1:9300".to_owned(),
        why: "the address is not a loopback address",
    };
    assert_eq!(named, &want);
    assert_eq!(
        refused.to_string(),
        "--tierstore-endpoint \"http://192.0.2.1:9300\" is not a tierstore on loopback: the \
         address is not a loopback address; give http://127.0.0.1:<port> or http://[::1]:<port>"
    );
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

/// Asserts that what the client cannot be made from stops the start, each refusal whole and
/// exactly, so that it names the flag and the file and never the secret: a credentials file
/// others may read, one that is not there, one that is not the key pair, and a bucket no
/// tierstore serves.
/// Catches a refusal that drops the flag or the path, or that carries the key file's contents.
#[test]
fn what_the_client_cannot_use_stops_the_start_with_the_flag_named() {
    let dir = scratch("tierstore-refused");
    tls_files(&dir);
    let good = keys(&dir, "keys.json", 0o600);
    let open = keys(&dir, "open.json", 0o644);
    let none = dir.join("none.json");
    let array = dir.join("array.json");
    std::fs::write(&array, format!("[\"{SECRET}\"]")).expect("write");
    std::fs::set_permissions(&array, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let refused = |flags: Vec<String>| started(&daemon(&dir, &flags)).expect_err("refused");
    let mut upper = on("http://127.0.0.1:9", &good);
    upper[1] = "--tierstore-bucket=UPPER".to_owned();
    let flag = "--tierstore-credentials-file";
    let cases = [
        (
            on("http://127.0.0.1:9", &open),
            format!(
                "{flag} {}: mode 0644 lets others read a secret: use 0600",
                open.display()
            ),
        ),
        (
            on("http://127.0.0.1:9", &none),
            format!(
                "{flag} {}: No such file or directory (os error 2)",
                none.display()
            ),
        ),
        (
            on("http://127.0.0.1:9", &array),
            format!(
                "{flag} {}: not JSON with access_key_id and secret_access_key",
                array.display()
            ),
        ),
        (
            upper,
            "--tierstore-bucket \"UPPER\": a bucket is 3 to 63 of a-z 0-9 . - and starts and ends \
             with a letter or digit"
                .to_owned(),
        ),
    ];
    for (flags, want) in cases {
        assert_eq!(refused(flags), want);
    }
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

/// The storage member of a `kbf storage-and-daemon`, served in this process from memory: no
/// listener is bound, as `start_member` refuses the flags before it reaches the member.
fn member() -> kbf_daemon::LocalMember {
    use kbf_storage::service::{Admission, Config, StorageService};
    let cache = Arc::new(kbf_front::Cache::memory());
    let storage = kbf_front::CacheStorage::new(cache, kbf_front::SegmentsConfig::DEFAULT);
    let config = Config {
        write_bytes_in_flight: 1 << 20,
    };
    let service = StorageService::new(Arc::new(storage), Admission::Open, config);
    kbf_daemon::LocalMember::new("127.0.0.1:1".to_owned(), Arc::new(service).in_process())
}

/// Asserts that the daemon half of a `kbf storage-and-daemon` with a frontend (`start_member`)
/// refuses `--tierstore-endpoint` at its start, the refusal exactly, where without it the same
/// daemon over the same member starts.
#[tokio::test]
async fn a_member_daemon_refuses_the_tierstore() {
    let dir = scratch("tierstore-member");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", 0o600);
    let started = |more: Vec<String>| {
        let mut cli = daemon(&dir, &more);
        cli.cas = None;
        let local = member();
        // `start_member` builds a runtime of its own, which tokio refuses on a thread of tasks.
        tokio::task::spawn_blocking(move || {
            crate::start_member(&cli, local)
                .map(drop)
                .map_err(|e| e.to_string())
        })
    };
    let refused = started(on("http://127.0.0.1:9", &path))
        .await
        .expect("joined");
    let want = "--tierstore-endpoint: only `kbf daemon` reads through a local tierstore, not \
                `kbf storage-and-daemon`";
    assert_eq!(refused, Err(want.to_owned()));
    assert_eq!(started(Vec::new()).await.expect("joined"), Ok(()));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts the bucket names a tierstore serves: 3 to 63 of `a-z 0-9 . -`, starting and ending
/// with a letter or a digit; and that the rest are refused.
#[test]
fn bucket_names_are_the_ones_s3_takes() {
    let longest = "b".repeat(63);
    for good in ["abc", "a1.b-2", "1ab", "ab9", longest.as_str()] {
        assert!(bucket_name(good), "{good}");
    }
    let too_long = "b".repeat(64);
    for bad in [
        "ab",
        too_long.as_str(),
        "-ab",
        "ab.",
        "aBc",
        "a_b",
        ".ab",
        "ab-",
    ] {
        assert!(!bucket_name(bad), "{bad}");
    }
}

/// Asserts that a key pair read from its file shows its id and never its secret.
#[test]
fn a_key_pair_never_shows_its_secret() {
    let dir = scratch("tierstore-key-pair");
    let path = keys(&dir, "keys.json", 0o600);
    let pair = pack_read::read_key_pair(&path, CREDENTIALS_FLAG).expect("a key pair");
    assert_eq!(
        format!("{pair:?}"),
        "KeyPair { id: \"AKIATIER\", secret: \"<redacted>\" }"
    );
    assert_eq!(pair.secret.as_str(), SECRET);
    kbf_outputs::remove_tree(&dir).expect("clean");
}
