//! The task flags: what they need, what they make, and what opening them checks.

use std::os::unix::fs::PermissionsExt as _;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use clap::error::ErrorKind;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_proto::reapi;
use kbf_storage::Storage;
use prost::Message as _;

use super::*;
use crate::frontend::{self, FrontendFlags};
use crate::test_dir::scratch_dir;

/// What a frontend needs of the command line apart from the task flags.
const BASE: [&str; 5] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--api-listen=127.0.0.1:0",
];

/// The five flags the switch needs.
const TASK: [&str; 5] = [
    "--task-secret-file=/etc/kbf/task.secret",
    "--task-put-endpoint=http://127.0.0.1:9000",
    "--task-put-bucket=tiers",
    "--task-put-credentials-file=/etc/kbf/presign.json",
    "--task-read-url=https://127.0.0.1:8443",
];

const LARGE_KEYS: &str = r#"{"access_key_id": "large-presign", "secret_access_key": "hunter3"}"#;
const LEASE: &str = "00000000000000010000000000000002";
const SECRET: &str = "0123456789abcdef0123456789abcdef";
const KEYS: &str = r#"{"access_key_id": "pack-presign", "secret_access_key": "hunter2",
    "ceiling": {"prefix": "cas/b/", "max_expires_secs": 14400, "max_object_bytes": 134217728}}"#;

/// The flags of a frontend given `more` after the base.
fn parsed(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let argv = BASE.iter().chain(more).copied();
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// The kind of error the flags are refused with.
fn refused(more: &[&str]) -> ErrorKind {
    parsed(more).expect_err("refused").kind()
}

/// Asserts that no run is prepared without `--task-secret-file`, and that the flags that have a
/// default, which the config template writes as live lines, change nothing and need nothing
/// when it is left out.
#[test]
fn the_task_flags_are_off_unless_the_secret_file_is_given() {
    assert_eq!(parsed(&[]).expect("parses").task, None);
    let companions = [
        "--task-put-region=eu-west-1",
        "--task-run-wall-secs=60",
        "--task-url-slack-secs=5",
        "--task-closure-max-digests=10",
        "--task-closure-max-bytes=1024",
        "--task-max-packs=2",
        "--task-max-large=2",
        "--task-large-max-bytes=100000000",
    ];
    assert_eq!(parsed(&companions).expect("parses").task, None);
    assert_eq!(
        refused(&["--task-large-credentials-file=/etc/kbf/large.json"]),
        ErrorKind::MissingRequiredArgument,
        "the large credentials need the switch"
    );
}

/// Asserts what each flag needs, so that the preparation is never half configured: the switch
/// needs the endpoint, the bucket, the credentials file and the read URL, and each of those
/// needs the switch.
#[test]
fn the_flags_need_each_other() {
    let missing = ErrorKind::MissingRequiredArgument;
    for (left_out, flag) in TASK.iter().enumerate() {
        let given: Vec<&str> = TASK
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != left_out)
            .map(|(_, flag)| *flag)
            .collect();
        assert_eq!(refused(&given), missing, "without {flag}");
    }
}

/// Asserts that the flags make the setting they say, and that the defaults are the frontend's:
/// region `us-east-1`, a wall of 2 hours and a slack of 10 minutes, half a million digests and
/// 32 GiB.
#[test]
fn the_flags_make_the_setting_and_the_defaults_are_the_constants() {
    let set = parsed(&TASK).expect("parses").task.expect("task flags");
    assert_eq!(set.secret_file, PathBuf::from("/etc/kbf/task.secret"));
    assert_eq!(set.put_endpoint, "http://127.0.0.1:9000");
    assert_eq!(set.put_bucket, "tiers");
    assert_eq!(
        set.put_credentials_file,
        PathBuf::from("/etc/kbf/presign.json")
    );
    assert_eq!(set.read_url, "https://127.0.0.1:8443");
    assert_eq!(set.put_region, DEFAULT_REGION);
    assert_eq!(set.run_wall, Duration::from_secs(DEFAULT_RUN_WALL_SECS));
    assert_eq!(set.url_slack, Duration::from_secs(DEFAULT_URL_SLACK_SECS));
    assert_eq!(set.max_digests, ClosureLimits::DEFAULT.max_digests);
    assert_eq!(set.max_bytes, ClosureLimits::DEFAULT.max_bytes);
    assert_eq!(set.max_packs, 8);
    assert_eq!(set.max_large, 8);
    assert_eq!(set.large_credentials_file, None);
    assert_eq!(set.large_max_bytes, 536_870_912);
    assert_eq!(set.large_max_bytes, DEFAULT_LARGE_MAX_BYTES);

    let mut every = TASK.to_vec();
    every.extend([
        "--task-put-region=eu-west-1",
        "--task-run-wall-secs=3600",
        "--task-url-slack-secs=0",
        "--task-closure-max-digests=1000",
        "--task-closure-max-bytes=1048576",
        "--task-max-packs=3",
        "--task-max-large=0",
        "--task-large-credentials-file=/etc/kbf/large.json",
        "--task-large-max-bytes=67108865",
    ]);
    let set = parsed(&every).expect("parses").task.expect("task flags");
    assert_eq!(set.put_region, "eu-west-1");
    assert_eq!(set.run_wall, Duration::from_secs(3_600));
    assert_eq!(set.url_slack, Duration::ZERO);
    assert_eq!(set.max_digests, 1_000);
    assert_eq!(set.max_bytes, 1 << 20);
    assert_eq!(set.max_packs, 3);
    assert_eq!(set.max_large, 0);
    assert_eq!(
        set.large_credentials_file,
        Some(PathBuf::from("/etc/kbf/large.json"))
    );
    assert_eq!(set.large_max_bytes, 64 * 1024 * 1024 + 1);
}

/// Asserts the bounds of the key-set flags: 1 to 8 packs, 0 to 8 large objects, and a large cap
/// above the 64 MiB a pack record may hold (a blob at the cap is a record, not a large object).
#[test]
fn the_key_set_flags_are_bounded() {
    for bad in [
        "--task-max-packs=0",
        "--task-max-packs=9",
        "--task-max-large=9",
        "--task-large-max-bytes=0",
        "--task-large-max-bytes=67108864",
        "--task-large-max-bytes=1024",
    ] {
        let mut given = TASK.to_vec();
        given.push(bad);
        assert_eq!(refused(&given), ErrorKind::ValueValidation, "{bad}");
    }
    let mut edges = TASK.to_vec();
    edges.extend([
        "--task-max-packs=1",
        "--task-max-large=8",
        "--task-large-max-bytes=67108865",
    ]);
    let set = parsed(&edges).expect("parses").task.expect("task flags");
    assert_eq!((set.max_packs, set.max_large), (1, 8));
    let mut edges = TASK.to_vec();
    edges.extend(["--task-max-packs=8", "--task-max-large=0"]);
    let set = parsed(&edges).expect("parses").task.expect("task flags");
    assert_eq!((set.max_packs, set.max_large), (8, 0));
}

/// Asserts the bounds of the numeric flags: the wall is 1 to 14400 seconds, the slack 0 to 14400,
/// and the budgets at least 1.
#[test]
fn the_numeric_flags_are_bounded() {
    for bad in [
        "--task-run-wall-secs=0",
        "--task-run-wall-secs=14401",
        "--task-url-slack-secs=14401",
        "--task-closure-max-digests=0",
        "--task-closure-max-bytes=0",
    ] {
        let mut given = TASK.to_vec();
        given.push(bad);
        assert_eq!(refused(&given), ErrorKind::ValueValidation, "{bad}");
    }
    let mut most = TASK.to_vec();
    most.extend(["--task-run-wall-secs=14400", "--task-url-slack-secs=14400"]);
    let set = parsed(&most).expect("parses").task.expect("task flags");
    assert_eq!(set.run_wall, Duration::from_secs(14_400));
}

/// Asserts that a read URL that is not `https://host[:port]` is refused when the flags are
/// parsed, which is at start, with the flag named.
#[test]
fn a_read_url_that_is_not_https_is_refused_at_start() {
    for url in [
        "http://127.0.0.1:8443",
        "https://127.0.0.1:8443/jobs",
        "127.0.0.1:8443",
        "https://",
    ] {
        let mut given = TASK[..4].to_vec();
        let flag = format!("--task-read-url={url}");
        given.push(&flag);
        let error = parsed(&given).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{url}");
        assert!(error.to_string().contains("--task-read-url"), "{url}");
    }
}

/// Asserts that `--executor-allow-plain-http-grant` opens nothing for a frontend that runs
/// placed actions on daemons: the opt-in is for `--executor js-agent`, and an `http://` read url
/// stays refused at start without it.
#[test]
fn the_plain_http_grant_opt_in_is_for_the_host_executor() {
    let mut given = TASK[..4].to_vec();
    given.push("--task-read-url=http://127.0.0.1:8443");
    given.push("--executor-allow-plain-http-grant");
    let error = parsed(&given).expect_err("refused");
    assert!(error.to_string().contains("--task-read-url"), "{error}");
}

/// The file called `name` in `dir` of this text and mode.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

/// Task flags over the files in `dir`: a private secret and a private credentials file.
fn flags(dir: &Path) -> TaskFlags {
    TaskFlags {
        secret_file: file(dir, "task.secret", &format!("{SECRET}\n"), 0o600),
        put_endpoint: "http://127.0.0.1:9000".to_owned(),
        put_bucket: "tiers".to_owned(),
        put_region: DEFAULT_REGION.to_owned(),
        put_credentials_file: file(dir, "presign.json", KEYS, 0o600),
        read_url: "https://127.0.0.1:8443".to_owned(),
        allow_plain_http_grant: false,
        run_wall: Duration::from_secs(DEFAULT_RUN_WALL_SECS),
        url_slack: Duration::from_secs(DEFAULT_URL_SLACK_SECS),
        max_digests: ClosureLimits::DEFAULT.max_digests,
        max_bytes: ClosureLimits::DEFAULT.max_bytes,
        max_packs: 8,
        max_large: 8,
        large_credentials_file: Some(file(dir, "large.json", LARGE_KEYS, 0o600)),
        large_max_bytes: 536_870_912,
    }
}

/// A store with an Action of an empty input root and a Command, and the Action's digest.
async fn farm() -> (Backend, kbf_types::Digest) {
    let store: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let command = reapi::Command {
        arguments: vec!["true".to_owned()],
        ..reapi::Command::default()
    };
    let command = store
        .put(Bytes::from(command.encode_to_vec()))
        .await
        .expect("command stored");
    let empty = kbf_segments::sha256(&[]);
    let action = reapi::Action {
        command_digest: Some(kbf_front::digest_to_proto(&command)),
        input_root_digest: Some(kbf_front::digest_to_proto(&empty)),
        ..reapi::Action::default()
    };
    let action = store
        .put(Bytes::from(action.encode_to_vec()))
        .await
        .expect("action stored");
    (Backend::remote(store), action)
}

/// Asserts that the flags open a preparer that prepares a run: the key is the one a mint of the
/// secret verifies, the write is for the endpoint and bucket, and the grant is for the read URL.
/// The secret file may end in a newline and the credentials file may carry the store's ceiling.
#[tokio::test]
async fn the_flags_open_a_preparer_that_prepares_a_run() {
    let dir = scratch_dir("task-open");
    let (backend, action) = farm().await;
    let prep = flags(dir.path()).open(backend).expect("opened");
    let now = UNIX_EPOCH + Duration::from_secs(1_790_000_040);
    let inputs = prep.prepare_run(LEASE, &action, now).await.expect("run");
    let mint = PackKeyMint::new(SECRET.as_bytes()).expect("a mint");
    assert!(mint.verify(&inputs.key, &action).is_ok());
    let prefix = format!("http://127.0.0.1:9000/tiers/{}?", inputs.key.key());
    assert!(
        inputs.put_url.starts_with(&prefix),
        "the write is for the key"
    );
    assert_eq!(inputs.read_grant.url, "https://127.0.0.1:8443");
    assert_eq!(inputs.read_grant.not_after, 1_790_000_040 + 7_200 + 600);
}

/// Asserts that no preparer is built without the switch, and that one is built with it.
#[tokio::test]
async fn a_preparer_is_built_only_when_the_switch_is_given() {
    let dir = scratch_dir("task-switch");
    let (backend, _) = farm().await;
    assert!(super::open(None, &backend).expect("opens").is_none());
    let set = flags(dir.path());
    assert!(super::open(Some(&set), &backend).expect("opens").is_some());
    let mut broken = flags(dir.path());
    broken.secret_file = dir.path().join("none");
    assert!(super::open(Some(&broken), &backend).is_err());
}

/// Asserts that a secret file others can read, a file that is not there and a secret under 32
/// bytes are refused with the file named or the length given and no part of the secret in the
/// text.
#[tokio::test]
async fn the_secret_comes_from_a_private_file_of_enough_bytes() {
    let dir = scratch_dir("task-secret");
    let (backend, _) = farm().await;
    let mut set = flags(dir.path());
    set.secret_file = file(dir.path(), "wide.secret", SECRET, 0o640);
    let said = set.open(backend.clone()).expect_err("refused").to_string();
    assert!(said.contains("wide.secret"), "{said}");
    assert!(said.contains("0640"), "{said}");
    assert!(!said.contains(SECRET), "{said}");

    set.secret_file = dir.path().join("none.secret");
    let said = set.open(backend.clone()).expect_err("refused").to_string();
    assert!(said.contains("none.secret"), "{said}");

    let short = "0123456789abcdef0123456789abcde";
    set.secret_file = file(dir.path(), "short.secret", short, 0o600);
    let error = set.open(backend).expect_err("refused");
    assert!(matches!(error, TaskFlagError::GrantKey(_)), "{error}");
    assert!(!error.to_string().contains(short), "{error}");
}

/// Asserts that a credentials file others can read, a file that is not there, a file that is not
/// the key and a key with an empty part are refused with the file named and no part of the
/// secret in the text.
#[tokio::test]
async fn the_credentials_come_from_a_private_file_of_a_key() {
    let dir = scratch_dir("task-credentials");
    let (backend, _) = farm().await;
    let mut set = flags(dir.path());
    let cases = [
        ("wide.json", KEYS, 0o640, "mode 0640"),
        ("junk.json", "hunter2 is not JSON", 0o600, "not JSON"),
        (
            "blank-id.json",
            r#"{"access_key_id": "", "secret_access_key": "hunter2"}"#,
            0o600,
            "both not empty",
        ),
        (
            "blank-secret.json",
            r#"{"access_key_id": "pack-presign", "secret_access_key": ""}"#,
            0o600,
            "both not empty",
        ),
        (
            "one-part.json",
            r#"{"access_key_id": "pack-presign"}"#,
            0o600,
            "not JSON",
        ),
    ];
    for (name, text, mode, expected) in cases {
        set.put_credentials_file = file(dir.path(), name, text, mode);
        let said = set.open(backend.clone()).expect_err("refused").to_string();
        assert!(said.contains(name), "{said}");
        assert!(said.contains(expected), "{name}: {said}");
        assert!(!said.contains("hunter2"), "{said}");
    }
    set.put_credentials_file = dir.path().join("none.json");
    let said = set.open(backend).expect_err("refused").to_string();
    assert!(said.contains("none.json"), "{said}");
}

/// Asserts what the preparer refuses at start that the flags alone cannot say: a wall and a slack
/// that pass the 4 hour ceiling of a presigned write, an endpoint that is not a URL and an empty
/// bucket.
#[tokio::test]
async fn the_preparer_refuses_what_the_flags_cannot_check_alone() {
    let dir = scratch_dir("task-config");
    let (backend, _) = farm().await;
    let mut long = flags(dir.path());
    long.run_wall = Duration::from_secs(14_400);
    long.url_slack = Duration::from_secs(1);
    let error = long.open(backend.clone()).expect_err("refused");
    assert!(matches!(error, TaskFlagError::Config(_)), "{error}");
    assert!(error.to_string().contains("14401"), "{error}");

    let mut endpoint = flags(dir.path());
    endpoint.put_endpoint = "127.0.0.1:9000".to_owned();
    assert!(endpoint.open(backend.clone()).is_err());

    let mut bucket = flags(dir.path());
    bucket.put_bucket = String::new();
    assert!(bucket.open(backend).is_err());
}

/// The URL's access key id: the first part of its credential scope.
fn access_key_id(url: &str) -> String {
    let (_, query) = url.split_once('?').expect("a query");
    let pair = query
        .split('&')
        .find(|pair| pair.starts_with("X-Amz-Credential="));
    let scope = pair
        .expect("a credential")
        .trim_start_matches("X-Amz-Credential=");
    scope.split("%2F").next().expect("an id").to_owned()
}

/// Asserts that the flags open a preparer whose runs get as many pack grants and large grants as
/// `--task-max-packs` and `--task-max-large` say, the large ones signed with the large
/// credentials file and the pack ones with the pack credentials file.
#[tokio::test]
async fn the_flags_open_a_preparer_that_gives_each_run_a_key_set() {
    let dir = scratch_dir("task-key-set");
    let (backend, action) = farm().await;
    let mut set = flags(dir.path());
    set.max_packs = 3;
    set.max_large = 2;
    let prep = set.open(backend).expect("opened");
    assert_eq!(prep.large_max_bytes(), Some(536_870_912));
    let now = UNIX_EPOCH + Duration::from_secs(1_790_000_040);
    let inputs = prep.prepare_run(LEASE, &action, now).await.expect("run");
    assert_eq!(inputs.packs.len(), 3);
    assert_eq!(inputs.large.len(), 2);
    let mint = PackKeyMint::new(SECRET.as_bytes()).expect("a mint");
    for key in inputs.keys() {
        assert!(mint.verify(&key, &action).is_ok(), "{}", key.key());
    }
    for grant in &inputs.packs {
        assert_eq!(access_key_id(&grant.put_url), "pack-presign");
    }
    for grant in &inputs.large {
        assert_eq!(access_key_id(&grant.put_url), "large-presign");
    }
}

/// Asserts that without `--task-large-credentials-file` no run gets a large key, whatever
/// `--task-max-large` says, and the packs are as many as asked.
#[tokio::test]
async fn without_the_large_credentials_file_a_run_has_no_large_keys() {
    let dir = scratch_dir("task-no-large");
    let (backend, action) = farm().await;
    let mut set = flags(dir.path());
    set.large_credentials_file = None;
    set.max_large = 8;
    let prep = set.open(backend).expect("opened");
    assert_eq!(prep.large_max_bytes(), None);
    let now = UNIX_EPOCH + Duration::from_secs(1_790_000_040);
    let inputs = prep.prepare_run(LEASE, &action, now).await.expect("run");
    assert_eq!(inputs.packs.len(), 8);
    assert!(inputs.large.is_empty());
}

/// Asserts that a large credentials file others can read, a file that is not there and a file
/// that is not a key are refused with the flag and the file named and no part of the secret in
/// the text.
#[tokio::test]
async fn the_large_credentials_come_from_a_private_file_of_a_key() {
    let dir = scratch_dir("task-large-credentials");
    let (backend, _) = farm().await;
    let mut set = flags(dir.path());
    let cases = [
        ("wide-large.json", LARGE_KEYS, 0o640, "mode 0640"),
        ("junk-large.json", "hunter3 is not JSON", 0o600, "not JSON"),
        (
            "blank-large.json",
            r#"{"access_key_id": "large-presign", "secret_access_key": ""}"#,
            0o600,
            "both not empty",
        ),
    ];
    for (name, text, mode, expected) in cases {
        set.large_credentials_file = Some(file(dir.path(), name, text, mode));
        let said = set.open(backend.clone()).expect_err("refused").to_string();
        assert!(said.contains("--task-large-credentials-file"), "{said}");
        assert!(said.contains(name), "{said}");
        assert!(said.contains(expected), "{name}: {said}");
        assert!(!said.contains("hunter3"), "{said}");
    }
    set.large_credentials_file = Some(dir.path().join("none-large.json"));
    let said = set.open(backend).expect_err("refused").to_string();
    assert!(said.contains("none-large.json"), "{said}");
}

/// Asserts that the preparer's own checks of the key set are reached from the flags: a large cap
/// at or under a pack record's, which the flag refuses too, does not open.
#[tokio::test]
async fn a_large_cap_at_a_pack_record_does_not_open() {
    let dir = scratch_dir("task-large-cap");
    let (backend, _) = farm().await;
    let mut set = flags(dir.path());
    set.large_max_bytes = 64 << 20;
    let error = set.open(backend).expect_err("refused");
    assert!(matches!(error, TaskFlagError::Config(_)), "{error}");
}
