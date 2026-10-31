//! The `--job-store-*` flags: the three come together and are off by default, the start reads
//! the grant key like the other secret files and refuses what could not sign a grant or name a
//! store, and a listener started with them sends grants in `GET /locate` (and one without them
//! sends none).

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::job_read::{BlobSource, Span};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;

use super::{
    Blobs, JobReadFlags, Serving, Timeouts, World, ca, files, flags, ip, prepare, refusal, request,
    world,
};
use crate::frontend::{StoreGrantFlags, command, parse};
use crate::test_dir::scratch_dir;

const ID: &str = "locate-grant";
const SECRET: &str = "locate-grant-secret-0123456789abcdef";
const URL: &str = "https://store.farm.test:9000";
const BUCKET: &str = "tiers";

fn key_file(dir: &Path, text: impl AsRef<[u8]>, mode: u32) -> std::path::PathBuf {
    let path = dir.join("grant.key");
    // A file at mode 0400 cannot be written over.
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, text).expect("key file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
    path
}

fn json_key() -> String {
    format!(r#"{{"access_key_id": "{ID}", "secret_access_key": "{SECRET}"}}"#)
}

fn with_store(mut flags: JobReadFlags, key: &Path, url: &str, bucket: &str) -> JobReadFlags {
    flags.store = Some(StoreGrantFlags {
        key_file: key.to_owned(),
        url: url.to_owned(),
        bucket: bucket.to_owned(),
    });
    flags
}

/// Catches: the three flags taken apart (a key with no store to name, a store with no key), a
/// store flag accepted with no listener, the flags being on by default, and the flags not
/// reaching the frontend's parsed flags.
#[test]
fn the_store_flags_come_together_and_are_off_by_default() {
    let base = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        "--job-listen=127.0.0.1:8990",
        "--job-plain-http",
        "--job-grant-secret-file=/etc/kbf/grant.secret",
    ];
    let store = [
        "--job-store-grant-key-file=/etc/kbf/store-grant.key",
        "--job-store-url=https://store.farm.test:9000",
        "--job-store-bucket=tiers",
    ];
    let with = |flags: &[&str]| {
        let argv = base.iter().chain(flags.iter()).copied();
        command().try_get_matches_from(argv).map(|m| parse(&m))
    };
    let none = with(&[]).expect("matches").expect("flags");
    assert!(none.job_read.expect("the listener").store.is_none());
    let all = with(&store).expect("matches").expect("flags");
    let found = all
        .job_read
        .expect("the listener")
        .store
        .expect("the store flags");
    assert_eq!(found.key_file, Path::new("/etc/kbf/store-grant.key"));
    assert_eq!(found.url, URL);
    assert_eq!(found.bucket, BUCKET);
    for (i, left_out) in store.iter().enumerate() {
        let rest: Vec<&str> = store.iter().filter(|f| f != &left_out).copied().collect();
        assert!(with(&rest).is_err(), "without {left_out} ({i})");
    }
    assert!(
        with(&[
            "--job-store-grant-key-file=/k",
            "--job-store-url=https://s",
            "--job-store-bucket="
        ])
        .is_err()
    );
    // No job listener: the store flags have nothing to serve.
    let argv = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--job-store-grant-key-file=/k",
    ]
    .into_iter()
    .chain(store[1..].iter().copied());
    assert!(command().try_get_matches_from(argv).is_err());
}

/// Catches: a key file anyone can read, one that is not a key (a JSON with another field or an
/// empty value, one line only), an id tierstore would refuse, and a secret in the refusal: a
/// refusal names the flag and never the file's text. JSON and the two-line form are both read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_reads_the_grant_key_as_the_other_secret_files_are_read() {
    let dir = scratch_dir("job-read-store-key");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let refused = |text: &str, mode: u32| {
        let key = key_file(dir.path(), text, mode);
        refusal(&with_store(flags(&files, "127.0.0.1:0"), &key, URL, BUCKET))
    };
    let why = refused(&json_key(), 0o644);
    assert!(
        why.contains("--job-store-grant-key-file") && why.contains("mode 0644"),
        "{why}"
    );
    for bad in [
        r#"{"access_key_id": "locate-grant", "secret_access_key": "locate-grant-secret-0123456789abcdef", "x": "y"}"#.to_owned(),
        r#"{"access_key_id": "locate-grant", "secret_access_key": ""}"#.to_owned(),
        r#"{"access_key_id": "", "secret_access_key": "locate-grant-secret-0123456789abcdef"}"#
            .to_owned(),
        r#"{"access_key_id": "locate-grant"}"#.to_owned(),
        ID.to_owned(),
        String::new(),
        format!("id/with/slash\n{SECRET}\n"),
    ] {
        let why = refused(&bad, 0o600);
        assert!(why.contains("--job-store-grant-key-file"), "{why}");
        assert!(!why.contains(SECRET), "{why}");
    }
    let not_text = key_file(dir.path(), [0xff, 0xfe, 0x00], 0o600);
    let why = refusal(&with_store(
        flags(&files, "127.0.0.1:0"),
        &not_text,
        URL,
        BUCKET,
    ));
    assert!(why.contains("--job-store-grant-key-file"), "{why}");
    let bucket = {
        let key = key_file(dir.path(), json_key(), 0o600);
        refusal(&with_store(
            flags(&files, "127.0.0.1:0"),
            &key,
            URL,
            "a bucket\nname",
        ))
    };
    assert!(bucket.contains("--job-store-bucket"), "{bucket}");
    // A name tierstore takes and the task's client refuses.
    for name in ["Tiers", "a_b", ".a", &"a".repeat(64)] {
        let key = key_file(dir.path(), json_key(), 0o600);
        let why = refusal(&with_store(flags(&files, "127.0.0.1:0"), &key, URL, name));
        assert!(why.contains("--job-store-bucket"), "{name}: {why}");
    }

    for text in [
        json_key(),
        format!("{ID}\n{SECRET}\n"),
        format!("{ID}\r\n{SECRET}\r\n"),
    ] {
        let key = key_file(dir.path(), &text, 0o400);
        let ok = prepare(&with_store(flags(&files, "127.0.0.1:0"), &key, URL, BUCKET));
        assert!(ok.is_ok(), "{text:?}");
    }
}

/// Catches: a grant key whose secret is under 32 bytes being accepted at the start and refused at
/// the first `GET /locate` that signs with it (every answer would then carry no grant); the
/// minimum resting on the tierstore pin rather than on the key reader's own check; and the
/// refusal naming the flag and the file without the secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_refuses_a_grant_secret_shorter_than_32_bytes() {
    let dir = scratch_dir("job-read-store-short-secret");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let short = "s".repeat(31);
    let key = key_file(dir.path(), format!("{ID}\n{short}\n"), 0o600);
    let why = refusal(&with_store(flags(&files, "127.0.0.1:0"), &key, URL, BUCKET));
    assert!(why.contains("--job-store-grant-key-file"), "{why}");
    assert!(why.contains("the secret is shorter than 32 bytes"), "{why}");
    assert!(!why.contains(&short), "{why}");
    let exact = "s".repeat(32);
    let key = key_file(dir.path(), format!("{ID}\n{exact}\n"), 0o600);
    let ok = prepare(&with_store(flags(&files, "127.0.0.1:0"), &key, URL, BUCKET));
    assert!(ok.is_ok());
}

/// Catches: a store URL a task could not use accepted at the start (and found out at the first
/// run): no http or https scheme, no host, a path, a query, a fragment, a space.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_refuses_a_store_url_a_task_cannot_use() {
    let dir = scratch_dir("job-read-store-url");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let key = key_file(dir.path(), json_key(), 0o600);
    for bad in [
        "ftp://store:9000",
        "store:9000",
        "https://",
        "http://store:9000/tiers",
        "http://store:9000?x=1",
        "http://store:9000#top",
        "http://sto re:9000",
        "http://user@store:9000",
        "http://store\\9000",
        "http://st\u{f6}re:9000",
        "http://sto\u{7}re:9000",
        "",
    ] {
        let why = refusal(&with_store(flags(&files, "127.0.0.1:0"), &key, bad, BUCKET));
        assert!(why.contains("--job-store-url"), "{bad}: {why}");
    }
    for good in [
        "http://127.0.0.1:9000",
        "https://store.farm.test",
        "https://[::1]:9000/",
    ] {
        let ok = prepare(&with_store(
            flags(&files, "127.0.0.1:0"),
            &key,
            good,
            BUCKET,
        ));
        assert!(ok.is_ok(), "{good}");
    }
}

/// A source that holds the world's blobs and says its input lies in a pack.
struct Located {
    blobs: Blobs,
    input: Digest,
}

impl BlobSource for Located {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        self.blobs.read_blob(digest)
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        self.blobs.read_stream(digest)
    }

    fn locate_packed<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Result<Vec<Option<Span>>, StorageError>> {
        let input = self.input;
        Box::pin(async move {
            let at = |d: &Digest| {
                (*d == input).then(|| Span {
                    key: "cas/f/w/1".to_owned(),
                    offset: 3,
                    length: d.size_bytes,
                })
            };
            Ok(digests.iter().map(at).collect())
        })
    }
}

/// The lines `GET /locate` answers over TLS to the grant of the world's Action, from a listener
/// started with `flags`.
async fn locate_lines(
    farm: &super::Ca,
    world: &World,
    flags: &JobReadFlags,
) -> Vec<serde_json::Value> {
    let prepared = prepare(flags).expect("prepared");
    let addr = prepared.local();
    let source = Arc::new(Located {
        blobs: Blobs(world.blobs.iter().cloned().collect()),
        input: world.input,
    });
    let Serving { latch, task, .. } = prepared
        .start(source, Timeouts::default())
        .expect("started");
    tokio::spawn(task);
    let got = request(addr, &farm.pem, "GET", "/locate", Some(&world.token()))
        .await
        .expect("connects");
    drop(latch);
    assert_eq!(got.status, 200, "{}", got.text());
    got.text()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect()
}

/// Catches: a listener started with the store flags that sends no header or no `g` (the flags
/// not reaching `GET /locate`), a header naming anything but the flags' store and bucket, a
/// grant that is not a token of the key in the file, and a listener started without them that
/// sends a header: the first line is the header, the located line names grant 0, and the
/// absent ones carry no `g`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_with_a_grant_key_sends_grants_with_its_table_and_one_without_sends_none() {
    let dir = scratch_dir("job-read-store-locate");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let key = key_file(dir.path(), json_key(), 0o600);

    let granting = with_store(flags(&files, "127.0.0.1:0"), &key, URL, BUCKET);
    let lines = locate_lines(&farm, &world, &granting).await;
    let header = &lines[0];
    assert_eq!(header["store"], URL);
    assert_eq!(header["bucket"], BUCKET);
    let tokens = header["grants"].as_array().expect("grants");
    assert_eq!(tokens.len(), 1);
    assert!(tokens[0].as_str().is_some_and(|t| t.contains('.')));
    let input = world.input.to_string();
    let line = |digest: &str| {
        lines[1..]
            .iter()
            .find(|l| l["digest"] == digest)
            .expect("a line")
    };
    assert_eq!(line(&input)["pack"]["key"], "cas/f/w/1");
    assert_eq!(line(&input)["g"], 0);
    let absent = world.action.to_string();
    assert_eq!(
        line(&absent),
        &serde_json::json!({"digest": absent, "absent": true})
    );
    assert!(
        lines[1..]
            .iter()
            .all(|l| l["digest"] != world.stranger.to_string())
    );

    let plain = locate_lines(&farm, &world, &flags(&files, "127.0.0.1:0")).await;
    assert!(
        plain
            .iter()
            .all(|l| l.get("grants").is_none() && l.get("g").is_none()),
        "{plain:?}"
    );
    assert_eq!(
        plain.iter().find(|l| l["digest"] == input).expect("a line")["pack"]["offset"],
        3
    );
}
