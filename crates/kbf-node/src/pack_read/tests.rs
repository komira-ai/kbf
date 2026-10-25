//! The `--pack-read` flags, the credentials file, and the starts that wrap the farm's data plane.

use std::os::unix::fs::PermissionsExt as _;

use clap::Parser as _;

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
/// the mode that reads packs, and takes the daemon without it.
#[test]
fn an_in_process_daemon_refuses_pack_read() {
    let on = parse(&ON).expect("flags").pack_read;
    let said = on.refuse_in_process().expect_err("refused").to_string();
    assert_eq!(
        said,
        "--pack-read: this daemon half reads blobs in-process; run `kbf daemon` to read packs"
    );
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
        let credentials = read_credentials(&path).expect(name);
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
        let failed = read_credentials(&path).expect_err("others read it");
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
        let said = read_credentials(path).expect_err("unusable").to_string();
        assert!(said.starts_with("--pack-store-credentials-file "), "{said}");
        assert!(said.contains(&path.display().to_string()), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }
    let array = read_credentials(&broken[3]).expect_err("an array");
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
    assert!(reader(&cli, &settings).is_ok());
    let cache = dir.join("cache");
    let cached = PackRead {
        cache: Some((cache.clone(), 1 << 20)),
        ..settings.clone()
    };
    assert!(reader(&cli, &cached).is_ok());
    assert!(cache.is_dir(), "the cache's directory was made");

    let refused = |settings: &PackRead, cli: &Cli| {
        let failed = reader(cli, settings).err().expect("refused");
        failed.to_string()
    };
    let tls = PackRead {
        endpoint: "https://127.0.0.1:9".to_owned(),
        ..settings.clone()
    };
    let said = refused(&tls, &cli);
    assert!(said.starts_with("--pack-store-endpoint: "), "{said}");
    let blocked = PackRead {
        cache: Some((path.clone(), 1 << 20)),
        ..settings.clone()
    };
    let said = refused(&blocked, &cli);
    assert!(said.starts_with("--pack-read-cache-dir: "), "{said}");
    let unreadable = PackRead {
        credentials: dir.join("nothing.json"),
        ..settings.clone()
    };
    let said = refused(&unreadable, &cli);
    assert!(said.starts_with("--pack-store-credentials-file "), "{said}");
    let mut without_cas = daemon(&dir, &on(&path));
    without_cas.cas = None;
    let said = refused(&settings, &without_cas);
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

/// Asserts that the daemons that read blobs in-process refuse `--pack-read` at their start: the
/// one beside a server's own process (`start_with`) and the one beside a storage node
/// (`start_member`), and that neither starts anything.
#[tokio::test]
async fn the_in_process_starts_refuse_pack_read() {
    let dir = scratch("pack-read-in-process");
    tls_files(&dir);
    let path = keys(&dir, "keys.json", &pair(), 0o600);
    let cli = daemon(&dir, &on(&path));
    let started = crate::start_with(&cli, crate::remote);
    let refused = started.err().expect("refused");
    assert!(refused.to_string().contains("--pack-read"), "{refused}");

    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
    ];
    let matches = kbf_server::storage_node::command().get_matches_from(argv);
    let flags = kbf_server::storage_node::parse(&matches).expect("the storage flags");
    let half = kbf_server::storage_half::start(&flags, || Ok(std::future::pending::<()>()))
        .await
        .expect("the storage half starts");
    let local = kbf_daemon::LocalMember::new(half.addr.to_string(), half.in_process());
    // `start_member` builds a runtime of its own, which tokio refuses on a thread running tasks.
    let start = tokio::task::spawn_blocking(move || {
        crate::start_member(&cli, local)
            .map(drop)
            .map_err(|e| e.to_string())
    });
    let said = start.await.expect("joined").expect_err("refused");
    assert!(said.contains("--pack-read"), "{said}");
    kbf_outputs::remove_tree(&dir).expect("clean");
}
