//! The `--pack-read` flags, the credentials file, and the starts that wrap the data planes.

use std::os::unix::fs::PermissionsExt as _;

use clap::Parser as _;
use kbf_objstore::MemoryStore;
use kbf_pack::{PackKeys, WriterKind};
use kbf_proto::reapi::Digest;
use kbf_server::storage_half::StorageHalf;
use kbf_transfer::CasError;

use super::*;
use crate::tests::{scratch, tls_files};

/// The secret of the credentials files of these tests, which no message may carry.
const SECRET: &str = "s3cr3t-key-material";

/// The flags of a daemon with the packs on, in the order the file lists them.
const ON: [&str; 4] = [
    "--pack-read",
    "--pack-store-endpoint=http://127.0.0.1:9",
    "--pack-store-bucket=packs",
    "--pack-store-credentials-file=/keys.json",
];

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

/// A key pair file in `dir` with this text and mode; its path.
fn keys(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// The text of a key pair file.
fn pair() -> String {
    format!(r#"{{"access_key_id": "AKIAEXAMPLE", "secret_access_key": "{SECRET}"}}"#)
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

/// The flags that turn the packs on, over the key pair file `credentials`.
fn on(credentials: &Path) -> Vec<String> {
    vec![
        "--pack-read".to_owned(),
        "--pack-store-endpoint=http://127.0.0.1:9".to_owned(),
        "--pack-store-bucket=packs".to_owned(),
        format!("--pack-store-credentials-file={}", credentials.display()),
    ]
}

/// Asserts that a daemon is as it was without `--pack-read`: no settings, the signing region
/// defaulted, and no companion given.
#[test]
fn the_packs_are_off_by_default() {
    let args = parse(&[]).expect("flags").pack_read;
    assert_eq!(args.settings(), None);
    assert!(!args.pack_read);
    assert_eq!(args.pack_store_region, "us-east-1");
    assert_eq!(args.pack_store_endpoint, None);
    assert_eq!(args.pack_store_bucket, None);
    assert_eq!(args.pack_store_credentials_file, None);
    assert_eq!(args.pack_read_cache_dir, None);
    assert_eq!(args.pack_read_cache_bytes, None);
}

/// Asserts that every flag reaches the settings: the endpoint, the bucket, the region (not the
/// default), the credentials file, and the cache's directory and size together.
#[test]
fn the_flags_reach_the_settings() {
    let mut flags = ON.to_vec();
    flags.extend([
        "--pack-store-region=eu-west-1",
        "--pack-read-cache-dir=/var/cache/packs",
        "--pack-read-cache-bytes=4096",
    ]);
    let settings = parse(&flags).expect("flags").pack_read.settings();
    let expected = PackRead {
        endpoint: "http://127.0.0.1:9".to_owned(),
        bucket: "packs".to_owned(),
        region: "eu-west-1".to_owned(),
        credentials: PathBuf::from("/keys.json"),
        cache: Some((PathBuf::from("/var/cache/packs"), 4096)),
        write: None,
    };
    assert_eq!(settings, Some(expected));
    let plain = parse(&ON).expect("flags").pack_read;
    let plain = plain.settings().expect("on");
    assert_eq!((plain.region.as_str(), plain.cache), ("us-east-1", None));
}

/// Asserts the refusals: the endpoint, the bucket, the credentials file and the cache's
/// directory and size are refused without `--pack-read`; `--pack-read` is refused without the
/// endpoint, the bucket or the credentials file; the cache's directory and size come together
/// and the size is at least one byte. The region has a default, which `kbf.toml` writes as a
/// line of its own, so it is taken alone.
#[test]
fn the_companions_come_with_pack_read_and_it_comes_with_them() {
    for alone in [
        "--pack-store-endpoint=http://127.0.0.1:9",
        "--pack-store-bucket=packs",
        "--pack-store-credentials-file=/keys.json",
        "--pack-read-cache-dir=/c",
        "--pack-read-cache-bytes=1",
    ] {
        assert!(parse(&[alone]).is_err(), "{alone} without --pack-read");
    }
    for left_out in 1..ON.len() {
        let flags: Vec<&str> = ON
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != left_out)
            .map(|(_, flag)| *flag)
            .collect();
        assert!(parse(&flags).is_err(), "{flags:?}");
    }
    let mut cached = ON.to_vec();
    cached.push("--pack-read-cache-dir=/c");
    assert!(parse(&cached).is_err(), "a directory without a size");
    let mut sized = ON.to_vec();
    sized.push("--pack-read-cache-bytes=1");
    assert!(parse(&sized).is_err(), "a size without a directory");
    cached.push("--pack-read-cache-bytes=0");
    assert!(parse(&cached).is_err(), "a cache of no bytes");
    assert!(parse(&["--pack-store-region=eu-west-1"]).is_ok());
}

/// Asserts that a daemon that reads blobs in-process refuses `--pack-read`, naming the flag and
/// the frontend it lacks, and takes the daemon without it.
#[test]
fn an_in_process_daemon_refuses_pack_read() {
    let on = parse(&ON).expect("flags").pack_read;
    let said = on.refuse_in_process().expect_err("refused").to_string();
    assert!(
        said.starts_with("--pack-read: no frontend to ask"),
        "{said}"
    );
    assert!(said.ends_with("storage-and-daemon --frontend`)"), "{said}");
    let off = parse(&[]).expect("flags").pack_read;
    assert!(off.refuse_in_process().is_ok());
}

/// Asserts that a key pair file is read when only its owner can read it (0600, 0400), and that the
/// key it gives shows its id and never its secret.
#[test]
fn a_key_pair_file_of_its_owner_alone_is_read() {
    let dir = scratch("pack-read-keys");
    for (name, mode) in [("rw.json", 0o600), ("r.json", 0o400)] {
        let path = keys(&dir, name, &pair(), mode);
        let credentials = read_credentials(&path, "--pack-store-credentials-file").expect(name);
        let shown = format!("{credentials:?}");
        assert!(shown.contains("AKIAEXAMPLE"), "{shown}");
        assert!(!shown.contains(SECRET), "{shown}");
    }
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a key pair file others can read is refused, naming the file and the mode, and
/// that no refusal of any file carries the secret: a file that is not there, a directory, text
/// that is not JSON, JSON that is not an object, and an object without a key or with a key that
/// is no string.
#[test]
fn a_key_pair_file_that_cannot_be_used_is_refused_without_the_secret() {
    let dir = scratch("pack-read-refused");
    for mode in [0o640, 0o604, 0o644, 0o666, 0o660] {
        let path = keys(&dir, "open.json", &pair(), mode);
        let failed =
            read_credentials(&path, "--pack-store-credentials-file").expect_err("others read it");
        let said = failed.to_string();
        assert!(said.contains(&path.display().to_string()), "{said}");
        assert!(said.contains(&format!("mode {mode:04o}")), "{said}");
        assert!(said.contains("lets others read a secret"), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }
    let nothing = dir.join("nothing.json");
    let directory = dir.join("a-directory");
    std::fs::create_dir(&directory).expect("directory");
    let broken = [
        nothing,
        directory,
        keys(&dir, "text.json", &format!("not json {SECRET}"), 0o600),
        keys(&dir, "array.json", &format!(r#"["{SECRET}"]"#), 0o600),
        keys(
            &dir,
            "no-secret.json",
            r#"{"access_key_id": "AKIAEXAMPLE"}"#,
            0o600,
        ),
        keys(
            &dir,
            "no-id.json",
            &format!(r#"{{"secret_access_key": "{SECRET}"}}"#),
            0o600,
        ),
        keys(
            &dir,
            "number.json",
            &format!(r#"{{"access_key_id": 7, "secret_access_key": "{SECRET}"}}"#),
            0o600,
        ),
    ];
    for path in &broken {
        let said = read_credentials(path, "--pack-store-credentials-file")
            .expect_err("unusable")
            .to_string();
        assert!(said.starts_with("--pack-store-credentials-file "), "{said}");
        assert!(said.contains(&path.display().to_string()), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }
    let array =
        read_credentials(&broken[3], "--pack-store-credentials-file").expect_err("an array");
    assert!(array.to_string().ends_with(NOT_KEYS), "{array}");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts what building the reader refuses, each naming its flag: an endpoint the S3 client
/// does not take (`https`), a daemon with no connection to the frontend to ask (`--cas` left
/// out), a read cache whose directory cannot be made (it is a file), and a key pair file that
/// cannot be used; and that with all of them well the reader is built, with the cache or without.
#[tokio::test]
async fn the_reader_is_built_from_the_settings_or_refused_by_flag() {
    let dir = scratch("pack-read-reader");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", &pair(), 0o600);
    let cli = daemon(&dir, &on(&path));
    let settings = cli.pack_read.settings().expect("on");
    let with = |locate: Locate| index(&cli, locate).expect("the index");
    assert!(reader(&settings, with(transfer::endpoint)).is_ok());
    assert!(reader(&settings, with(transfer::server_endpoint)).is_ok());
    let cache = dir.join("cache");
    let cached = PackRead {
        cache: Some((cache.clone(), 1 << 20)),
        ..settings.clone()
    };
    assert!(reader(&cached, with(transfer::endpoint)).is_ok());
    assert!(cache.is_dir(), "the cache's directory was made");

    let refused = |settings: &PackRead| {
        let built = reader(settings, with(transfer::endpoint));
        built.err().expect("refused").to_string()
    };
    let tls = PackRead {
        endpoint: "https://127.0.0.1:9".to_owned(),
        ..settings.clone()
    };
    let said = refused(&tls);
    assert!(said.starts_with("--pack-store-endpoint: "), "{said}");
    let blocked = PackRead {
        cache: Some((path.clone(), 1 << 20)),
        ..settings.clone()
    };
    let said = refused(&blocked);
    assert!(said.starts_with("--pack-read-cache-dir: "), "{said}");
    let unreadable = PackRead {
        credentials: dir.join("nothing.json"),
        ..settings.clone()
    };
    let said = refused(&unreadable);
    assert!(said.starts_with("--pack-store-credentials-file "), "{said}");
    let mut without_cas = daemon(&dir, &on(&path));
    without_cas.cas = None;
    let said = index(&without_cas, transfer::endpoint)
        .err()
        .expect("refused")
        .to_string();
    assert!(said.contains("--cas is required"), "{said}");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that `start` with `--pack-read` makes the daemon: the data plane is made with the
/// packs in front of it when a driver that moves blobs is set up, and a key pair file that
/// cannot be used stops the start with the file named, not the first read.
#[test]
fn a_daemon_starts_with_the_packs_in_front_of_its_plane_or_is_refused() {
    let dir = scratch("pack-read-start");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", &pair(), 0o600);
    let cli = daemon(&dir, &on(&path));
    drop(crate::start(&cli).expect("starts"));

    let open = keys(&dir, "open.json", &pair(), 0o644);
    let cli = daemon(&dir, &on(&open));
    let refused = crate::start(&cli).err().expect("refused");
    assert!(refused.to_string().contains("open.json"), "{refused}");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that the daemon `start` makes with `--pack-read` is given the counter its data plane
/// counts into, so its heartbeats carry the counts, and that without `--pack-read` it is given
/// none, so its heartbeats carry none.
#[test]
fn a_daemon_carries_the_pack_counts_only_with_pack_read() {
    let dir = scratch("pack-read-counts");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", &pair(), 0o600);
    let packed = crate::start(&daemon(&dir, &on(&path))).expect("starts");
    let counted = packed.daemon.pack_reads().is_some();
    drop(packed);
    let plain = crate::start(&daemon(&dir, &[])).expect("starts");
    let uncounted = plain.daemon.pack_reads().is_none();
    drop(plain);
    assert!(counted);
    assert!(uncounted);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// A data plane that holds no blob.
struct Empty;

impl Cas for Empty {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        Err(CasError::Missing(digest.hash.clone()))
    }

    async fn put(&self, _: Vec<u8>) -> Result<Digest, CasError> {
        Err(CasError::Unavailable("-".into(), "no CAS".into()))
    }
}

/// Asserts that the plane `over` makes counts into the counter it is given: a read of a blob
/// whose hash is no SHA-256 digest goes to the plane underneath and is counted as read
/// elsewhere in that counter, and in nothing else.
#[tokio::test]
async fn the_plane_counts_into_the_counter_it_is_given() {
    let dir = scratch("pack-read-over");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", &pair(), 0o600);
    let cli = daemon(&dir, &on(&path));
    let settings = cli.pack_read.settings().expect("on");
    let counter = PackCounter::default();
    let empty = Arc::new(Empty);
    let plane = over(&cli, &settings, empty, transfer::endpoint, &counter, None);
    let plane = plane.expect("the plane");
    let digest = Digest {
        hash: "not-hex".to_owned(),
        size_bytes: 1,
    };
    let read = plane.get(&digest).await;
    assert!(matches!(read, Err(CasError::Missing(_))), "{read:?}");
    let stats = counter.stats();
    assert_eq!(stats.elsewhere, 1);
    assert_eq!(stats.served + stats.unasked + stats.lost + stats.damaged, 0);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that the daemon that reads blobs in-process beside a server's own process
/// (`start_with`: `kbf all-in-one`, and `kbf storage-and-daemon` without a `--frontend`) refuses
/// `--pack-read` at its start, naming the flag, and starts nothing.
#[test]
fn the_in_process_start_refuses_pack_read() {
    let dir = scratch("pack-read-in-process");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", &pair(), 0o600);
    let cli = daemon(&dir, &on(&path));
    let started = crate::start_with(&cli, crate::remote);
    let refused = started.err().expect("refused");
    assert!(refused.to_string().contains("--pack-read"), "{refused}");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// A storage half on a loopback port, and the member it is for the daemon half beside it. The
/// half is kept by the caller for as long as the member is used.
async fn storage_member(dir: &Path) -> (StorageHalf, LocalMember) {
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--meta-dir={}", dir.join("meta").display()),
    ];
    let matches = kbf_server::storage_node::command().get_matches_from(argv);
    let flags = kbf_server::storage_node::parse(&matches).expect("the storage flags");
    let half = kbf_server::storage_half::start(&flags, || Ok(std::future::pending::<()>()))
        .await
        .expect("the storage half starts");
    let local = LocalMember::new(half.addr.to_string(), half.in_process());
    (half, local)
}

/// `start_member` over `local` for a daemon with these more flags, no `--cas` (a
/// storage-and-daemon has none) and these `--server` URLs: nothing, or what it refused with.
async fn started_member(
    dir: &Path,
    more: &[String],
    servers: &[&str],
    local: &LocalMember,
) -> Result<(), String> {
    let mut cli = daemon(dir, more);
    cli.cas = None;
    cli.daemon.servers = servers.iter().map(|url| (*url).to_owned()).collect();
    let local = local.clone();
    // `start_member` builds a runtime of its own, which tokio refuses on a thread running tasks.
    let start = tokio::task::spawn_blocking(move || {
        crate::start_member(&cli, local)
            .map(drop)
            .map_err(|e| e.to_string())
    });
    start.await.expect("joined")
}

/// Asserts that `start_member` with `--pack-read` makes the daemon half of a storage-and-daemon
/// with the packs in front of its member plane, without a `--cas`: the start is refused with
/// the file named for a key pair file that cannot be used, and with `--server` named for a
/// frontend that is not an https URL, before any blob is read; without `--pack-read` the same
/// daemon starts as it did.
#[tokio::test]
async fn a_member_daemon_starts_with_the_packs_in_front_of_its_plane_or_is_refused() {
    let dir = scratch("pack-read-member");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", &pair(), 0o600);
    let open = keys(&dir, "open.json", &pair(), 0o644);
    let (_half, local) = storage_member(&dir).await;
    let https = ["https://127.0.0.1:1"];

    let started = started_member(&dir, &on(&path), &https, &local).await;
    assert_eq!(started, Ok(()));
    let started = started_member(&dir, &on(&open), &https, &local).await;
    let said = started.expect_err("refused");
    assert!(said.contains("open.json"), "{said}");
    let started = started_member(&dir, &on(&path), &["http://127.0.0.1:1"], &local).await;
    let said = started.expect_err("refused");
    assert!(
        said.starts_with("--server \"http://127.0.0.1:1\" must be an https:// URL"),
        "{said}"
    );
    let started = started_member(&dir, &[], &https, &local).await;
    assert_eq!(started, Ok(()));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a daemon with no `--cas` asks the frontend over its first `--server` (the
/// listener its session is made to, with its own TLS files), and that a first `--server` that
/// is not an https URL, or none, is refused with the flag named.
#[test]
fn the_frontend_is_asked_over_the_first_server() {
    let dir = scratch("pack-read-server");
    tls_files(&dir);
    let mut cli = daemon(&dir, &[]);
    cli.daemon.servers = vec![
        "https://127.0.0.1:2".to_owned(),
        "https://127.0.0.1:3".to_owned(),
    ];
    let endpoint = transfer::server_endpoint(&cli).expect("the first server");
    assert_eq!(endpoint.uri().port_u16(), Some(2));

    cli.daemon.servers = vec!["http://127.0.0.1:2".to_owned()];
    let said = transfer::server_endpoint(&cli).expect_err("not https");
    assert!(said.to_string().starts_with("--server "), "{said}");
    cli.daemon.servers = Vec::new();
    let said = transfer::server_endpoint(&cli).expect_err("none");
    assert_eq!(said.to_string(), "--server is required");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// The flags that turn the packs on and writing to them on, over the key pair files `read` and
/// `write`.
fn writing(read: &Path, write: &Path) -> Vec<String> {
    let mut flags = on(read);
    flags.push("--pack-write".to_owned());
    flags.push(format!(
        "--pack-store-write-credentials-file={}",
        write.display()
    ));
    flags
}

/// Asserts that writing packs is off by default: `--pack-write` is false, no write key file is
/// given, and the settings of a daemon that reads packs ask for no write.
#[test]
fn packs_are_not_written_by_default() {
    let args = parse(&[]).expect("flags").pack_read;
    assert!(!args.pack_write);
    assert_eq!(args.pack_store_write_credentials_file, None);
    let read = parse(&ON).expect("flags").pack_read;
    assert_eq!(read.settings().expect("on").write, None);
}

/// Asserts the refusals of `--pack-write`: it is refused without `--pack-read`, naming it, and
/// without the key file it writes with, naming that; the key file is refused without
/// `--pack-write`; with all of them the key file reaches the settings.
#[test]
fn pack_write_needs_pack_read_and_its_own_key_file() {
    let said = |flags: &[&str]| parse(flags).expect_err("refused").to_string();
    let alone = said(&[
        "--pack-write",
        "--pack-store-write-credentials-file=/w.json",
    ]);
    assert!(alone.contains("--pack-read"), "{alone}");
    let mut no_key = ON.to_vec();
    no_key.push("--pack-write");
    let said_no_key = said(&no_key);
    assert!(
        said_no_key.contains("--pack-store-write-credentials-file"),
        "{said_no_key}"
    );
    let mut no_write = ON.to_vec();
    no_write.push("--pack-store-write-credentials-file=/w.json");
    let said_no_write = said(&no_write);
    assert!(said_no_write.contains("--pack-write"), "{said_no_write}");
    let mut all = ON.to_vec();
    all.extend([
        "--pack-write",
        "--pack-store-write-credentials-file=/w.json",
    ]);
    let args = parse(&all).expect("flags").pack_read;
    assert!(args.pack_write);
    let settings = args.settings().expect("on");
    assert_eq!(settings.write, Some(PathBuf::from("/w.json")));
}

/// Asserts that a start with `--pack-write` builds the writer: a write key file that cannot be
/// used stops the start with `--pack-store-write-credentials-file` and the file named, and a
/// store that cannot be reached for the create-only check stops it naming `--pack-write`. The
/// read key file is not the one that was refused.
#[test]
fn a_daemon_that_writes_packs_is_refused_by_the_writer_it_builds() {
    let dir = scratch("pack-write-start");
    tls_files(&dir);
    let read = keys(&dir, "read.json", &pair(), 0o600);
    let write = keys(&dir, "write.json", &pair(), 0o600);
    let open = keys(&dir, "open.json", &pair(), 0o644);

    let cli = daemon(&dir, &writing(&read, &open));
    let said = crate::start(&cli).err().expect("refused").to_string();
    assert!(
        said.starts_with("--pack-store-write-credentials-file "),
        "{said}"
    );
    assert!(said.contains("open.json"), "{said}");

    let cli = daemon(&dir, &writing(&read, &write));
    let said = crate::start(&cli).err().expect("refused").to_string();
    assert!(said.starts_with("--pack-write: the pack store"), "{said}");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that the daemon `start` makes is given a commit handle to set from its `Welcome`s
/// only when writing packs is asked for: not by `start` without `--pack-write`, and by
/// `counting` with the `Write` of settings that ask for it.
#[test]
fn a_daemon_is_given_the_commit_handle_only_with_pack_write() {
    let dir = scratch("pack-write-handle");
    tls_files(&dir);
    let read = keys(&dir, "read.json", &pair(), 0o600);
    let node = crate::start(&daemon(&dir, &on(&read))).expect("starts");
    assert!(node.daemon.pack_writes().is_none());

    let settings = PackRead {
        write: Some(PathBuf::from("/w.json")),
        ..parse(&ON).expect("flags").pack_read.settings().expect("on")
    };
    let write = Write::of(&settings).expect("asked for");
    let commit = write.commit.clone();
    let node = counting(node, PackCounter::default(), Some(write));
    let given = node.daemon.pack_writes().cloned();
    drop(node);
    let given = given.expect("given the handle");
    commit.welcomed(true);
    assert!(given.enabled(), "it is the handle the plane is given");
    assert!(
        Write::of(&PackRead {
            write: None,
            ..settings
        })
        .is_none()
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts the check at start: a store that claims create-only writes gives a writer, and one
/// that does not (the default capabilities) is refused naming `--pack-write`.
#[tokio::test]
async fn the_writer_is_built_over_a_store_that_refuses_to_replace_objects() {
    let dir = scratch("pack-write-over");
    tls_files(&dir);
    let read = keys(&dir, "read.json", &pair(), 0o600);
    let cli = daemon(&dir, &on(&read));
    let keys = || PackKeys::new(WriterKind::Daemon, "n1", "boot1").expect("keys");
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let index = || index(&cli, transfer::endpoint).expect("the index");
    let store = Arc::new(MemoryStore::new(create_only));
    assert!(writer_over(store, index(), keys()).await.is_ok());
    let store = Arc::new(MemoryStore::new(Capabilities::default()));
    let said = writer_over(store, index(), keys())
        .await
        .err()
        .expect("refused");
    assert!(
        said.to_string().starts_with("--pack-write: the pack store"),
        "{said}"
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that a node id a pack key cannot hold is refused naming `--node-id`, before the store
/// is contacted, and that the name of a start ends with the process id.
#[tokio::test]
async fn a_node_id_that_cannot_name_packs_is_refused() {
    let dir = scratch("pack-write-name");
    tls_files(&dir);
    let read = keys(&dir, "read.json", &pair(), 0o600);
    let write = keys(&dir, "write.json", &pair(), 0o600);
    let mut cli = daemon(&dir, &writing(&read, &write));
    cli.daemon.node_id = "a/b".to_owned();
    let index = index(&cli, transfer::endpoint).expect("the index");
    let store = Arc::new(MemoryStore::new(Capabilities::default()));
    let said = pack_writer(&cli, store, index).err().expect("refused");
    assert!(said.to_string().starts_with("--node-id: "), "{said}");
    assert!(boot_id().ends_with(&format!("-{:x}", std::process::id())));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Asserts that the plane `over_store` makes with a write is given the writer and its handle: the
/// store is made from the settings and the write key file, and a put while the handle is off is
/// counted as a write that went to the plane underneath (the plane has no writer otherwise, and
/// counts none), and fails as that plane does.
#[tokio::test]
async fn the_plane_over_a_write_is_given_the_writer_and_its_handle() {
    let dir = scratch("pack-write-plane");
    tls_files(&dir);
    let read = keys(&dir, "read.json", &pair(), 0o600);
    let write = keys(&dir, "write.json", &pair(), 0o600);
    let cli = daemon(&dir, &writing(&read, &write));
    let settings = cli.pack_read.settings().expect("on");
    let asked = Write::of(&settings).expect("asked for");
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    // The check at start blocks on the runtime, which tokio refuses on a thread running tasks.
    let built = tokio::task::spawn_blocking(move || {
        let made = std::cell::Cell::new(None);
        let make = |_: &PackRead, file: &Path| {
            made.set(Some(file.to_path_buf()));
            Ok(Arc::new(MemoryStore::new(create_only)))
        };
        let counter = PackCounter::default();
        let plane = Arc::new(Empty);
        let built = over_store(
            &cli,
            &settings,
            Arc::clone(&plane),
            transfer::endpoint,
            &counter,
            Some(&asked),
            make,
        );
        let refused =
            |_: &PackRead, _: &Path| -> Result<Arc<MemoryStore>, Error> { Err("no".into()) };
        let none = over_store(
            &cli,
            &settings,
            plane,
            transfer::endpoint,
            &counter,
            Some(&asked),
            refused,
        );
        let none = none.err().map(|e| e.to_string());
        (built.ok(), made.take(), none)
    });
    let (plane, made, none) = built.await.expect("joined");
    assert_eq!(made, Some(write.clone()));
    assert_eq!(none.as_deref(), Some("no"));
    let plane = plane.expect("the plane");
    let put = plane.put(b"output".to_vec()).await;
    assert!(matches!(put, Err(CasError::Unavailable(..))), "{put:?}");
    assert_eq!(plane.stats().writes.fallbacks.off, 1);
    kbf_outputs::remove_tree(&dir).expect("clean");
}
