//! The janitor's flags and key file, the janitor they open over a store on loopback and the
//! operations' rows of a state store, and a frontend started with them.

use std::os::unix::fs::PermissionsExt as _;

use bytes::Bytes;
use clap::error::ErrorKind;
use kbf_objstore::ObjectKey;
use kbf_objstore::s3::fake::FakeS3;
use kbf_pack::{DEFAULT_GRACE, Slot};
use kbf_types::OperationId;

use super::tests::{dir_of, in_dir, key};
use super::*;
use crate::executor::rows::OpRow;
use crate::executor::tests::Faulty;
use crate::frontend::pack_tests::{flags_of, index, pack_flags, scrape, started};
use crate::frontend::{self, FrontendFlags};
use crate::test_dir::scratch_dir;

/// What a frontend needs of the command line apart from the janitor flags.
const BASE: [&str; 5] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--api-listen=127.0.0.1:0",
];

/// The flags the task switch needs.
const TASK: [&str; 2] = [
    "--task-secret-file=/etc/kbf/task.secret",
    "--task-out-root=/mnt/ts/put",
];

/// The flags the packs need.
const PACKS: [&str; 5] = [
    "--state-store=postgres:///kbf?host=/run/postgresql",
    "--pack-index",
    "--pack-store-endpoint=http://127.0.0.1:9000",
    "--pack-store-bucket=tiers",
    "--pack-store-credentials-file=/etc/kbf/pack.json",
];

const KEY_FILE: &str = "--task-janitor-key-file=/etc/kbf/gc.json";

fn parsed(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let argv = BASE.iter().chain(more).copied();
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// The task and pack flags the janitor needs, then `janitor`.
fn with<'a>(janitor: &[&'a str]) -> Vec<&'a str> {
    let mut all: Vec<&str> = TASK.iter().chain(&PACKS).copied().collect();
    all.extend(janitor);
    all
}

fn refused(more: &[&str]) -> ErrorKind {
    parsed(more).expect_err("refused").kind()
}

/// Asserts that no janitor exists without the key file, whatever its companions say, and that
/// the key file needs the task switch (clap) and the packs (the parse).
#[test]
fn the_janitor_is_off_unless_the_key_file_is_given() {
    let companions = [
        "--task-janitor-interval-secs=5",
        "--task-janitor-grace-secs=21600",
        "--task-janitor-page-size=10",
        "--task-janitor-page-pause-ms=0",
        "--task-janitor-dry-run",
    ];
    assert_eq!(parsed(&companions).expect("parses").janitor, None);
    assert_eq!(parsed(&with(&[])).expect("parses").janitor, None);
    assert_eq!(
        refused(&[KEY_FILE]),
        ErrorKind::MissingRequiredArgument,
        "the key file needs the task switch"
    );
    assert_eq!(
        refused(&with(&["--task-janitor-apply"])),
        ErrorKind::MissingRequiredArgument,
        "apply needs the key file"
    );
    let tasks: Vec<&str> = TASK.iter().copied().chain([KEY_FILE]).collect();
    let said = parsed(&tasks).expect_err("refused");
    assert_eq!(said.kind(), ErrorKind::ArgumentConflict);
    assert!(said.to_string().contains("--pack-store-endpoint"), "{said}");
}

/// Asserts that the flags make the setting they say and that the defaults are the constants:
/// the grace of the sweep, 15 minutes between sweeps, pages of 500 and a second between them,
/// and a dry run unless `--task-janitor-apply` is given.
#[test]
fn the_flags_make_the_setting_and_the_default_is_a_dry_run() {
    let set = parsed(&with(&[KEY_FILE])).expect("parses");
    let set = set.janitor.expect("janitor flags");
    assert_eq!(set.key_file, PathBuf::from("/etc/kbf/gc.json"));
    assert_eq!(set.interval, Duration::from_secs(DEFAULT_INTERVAL_SECS));
    assert_eq!(set.grace, DEFAULT_GRACE);
    assert_eq!(set.page.get(), DEFAULT_PAGE_SIZE);
    assert_eq!(set.pause, Duration::from_millis(DEFAULT_PAGE_PAUSE_MS));
    assert!(!set.apply, "a dry run by default");

    let every = [
        KEY_FILE,
        "--task-janitor-interval-secs=30",
        "--task-janitor-grace-secs=21600",
        "--task-janitor-page-size=7",
        "--task-janitor-page-pause-ms=0",
        "--task-janitor-apply",
    ];
    let set = parsed(&with(&every)).expect("parses").janitor;
    let set = set.expect("janitor flags");
    assert_eq!(set.interval, Duration::from_secs(30));
    assert_eq!(set.grace, Duration::from_secs(21_600));
    assert_eq!(set.page.get(), 7);
    assert_eq!(set.pause, Duration::ZERO);
    assert!(set.apply);
}

/// Asserts that `--task-janitor-dry-run`, which earlier releases took to turn deletes off, is
/// still accepted from the command line and from a config file and makes a dry run, is left out
/// of `--help`, and is refused beside `--task-janitor-apply`, which says the opposite.
#[test]
fn the_dry_run_flag_of_earlier_releases_is_accepted_and_refused_beside_apply() {
    let set = parsed(&with(&[KEY_FILE, "--task-janitor-dry-run"])).expect("parses");
    assert!(!set.janitor.expect("janitor flags").apply);
    let both = with(&[KEY_FILE, "--task-janitor-dry-run", "--task-janitor-apply"]);
    assert_eq!(refused(&both), ErrorKind::ArgumentConflict);
    let help = frontend::command().render_long_help().to_string();
    assert!(help.contains("--task-janitor-apply"), "{help}");
    assert!(!help.contains("--task-janitor-dry-run"), "{help}");
}

/// Asserts the bounds: a grace under 6 hours (the age an ingest refuses a key at, and an hour of
/// margin for a fenced old leader that is still ingesting) is refused, and the least is exactly
/// that; as are an interval of 0, a page of 0 or over 1000, and a pause over a minute.
#[test]
fn the_flags_hold_to_their_bounds() {
    let margin = Duration::from_secs(3_600);
    assert_eq!(MIN_GRACE_SECS, (kbf_pack::MIN_GRACE + margin).as_secs());
    let bad = [
        "--task-janitor-grace-secs=21599",
        "--task-janitor-grace-secs=604801",
        "--task-janitor-interval-secs=0",
        "--task-janitor-interval-secs=86401",
        "--task-janitor-page-size=0",
        "--task-janitor-page-size=1001",
        "--task-janitor-page-pause-ms=60001",
    ];
    for flag in bad {
        assert_eq!(
            refused(&with(&[KEY_FILE, flag])),
            ErrorKind::ValueValidation,
            "{flag}"
        );
    }
    let edges = [
        "--task-janitor-grace-secs=21600",
        "--task-janitor-page-size=1000",
        "--task-janitor-page-size=1",
    ];
    for flag in edges {
        assert!(parsed(&with(&[KEY_FILE, flag])).is_ok(), "{flag}");
    }
}

/// The file called `name` in `dir` of this text and mode.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

const GC_KEY: &str =
    r#"{"access_key_id": "pack-gc", "secret_access_key": "hunter2", "prefix": "cas/b/"}"#;

/// What reading the key file at `path` says, as text.
fn said(path: &Path) -> String {
    match read_key(path) {
        Ok(_) => "accepted".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// Asserts what the key file must be: the JSON of exactly the three fields of the tier store's
/// GC key, with the prefix exactly `cas/b/` (the janitor is given no other reach), owned by this
/// user and private; and that a refusal names the flag and the file and never the secret.
#[test]
fn the_key_file_is_the_gc_key_of_cas_b_and_private() {
    let dir = scratch_dir("janitor-key");
    assert_eq!(
        said(&file(dir.path(), "ok.json", GC_KEY, 0o600)),
        "accepted"
    );
    assert_eq!(
        said(&file(dir.path(), "ro.json", GC_KEY, 0o400)),
        "accepted"
    );

    let other = |prefix: &str| {
        let body = format!(
            r#"{{"access_key_id": "k", "secret_access_key": "hunter2", "prefix": "{prefix}"}}"#
        );
        said(&file(dir.path(), "prefix.json", &body, 0o600))
    };
    for prefix in ["cas/", "cas/l/", "cas/b", "cas/b/x/", ""] {
        let text = other(prefix);
        assert!(text.contains("must be exactly"), "{prefix:?}: {text}");
        assert!(!text.contains("hunter2"));
    }
    let bad = [
        ("open.json", GC_KEY, 0o644, "mode"),
        ("empty.json", "", 0o600, "exactly access_key_id"),
        (
            "extra.json",
            r#"{"access_key_id": "k", "secret_access_key": "hunter2", "prefix": "cas/b/", "x": 1}"#,
            0o600,
            "exactly access_key_id",
        ),
        (
            "no-id.json",
            r#"{"access_key_id": "", "secret_access_key": "hunter2", "prefix": "cas/b/"}"#,
            0o600,
            "exactly access_key_id",
        ),
        (
            "no-secret.json",
            r#"{"access_key_id": "k", "secret_access_key": "", "prefix": "cas/b/"}"#,
            0o600,
            "exactly access_key_id",
        ),
        (
            "no-prefix.json",
            r#"{"access_key_id": "k", "secret_access_key": "hunter2"}"#,
            0o600,
            "exactly access_key_id",
        ),
    ];
    for (name, body, mode, want) in bad {
        let text = said(&file(dir.path(), name, body, mode));
        assert!(text.starts_with("--task-janitor-key-file "), "{text}");
        assert!(text.contains(name), "{text}");
        assert!(text.contains(want), "{name}: {text}");
        assert!(!text.contains("hunter2"), "{text}");
    }
    let missing = said(&dir.path().join("none.json"));
    assert!(missing.starts_with("--task-janitor-key-file "), "{missing}");
}

/// The janitor flags with a key file in `dir`, a sweep every minute, nothing paused.
fn janitor_flags(dir: &Path) -> JanitorFlags {
    JanitorFlags {
        key_file: file(dir, "gc.json", GC_KEY, 0o600),
        interval: Duration::from_secs(60),
        grace: DEFAULT_GRACE,
        page: PageSize::new(2).expect("a page"),
        pause: Duration::ZERO,
        apply: true,
    }
}

/// Pack flags for the S3 service at `endpoint`, and packs over them.
fn packs_at(endpoint: &str, dir: &Path) -> (PackFlags, Packs) {
    let flags = flags_of(&pack_flags(endpoint, dir))
        .packs
        .expect("pack flags");
    let packs = flags.open(index()).expect("packs");
    (flags, packs)
}

/// Asserts that the janitor opened from the flags signs its requests with the key file's key and
/// no other, against the pack store: a store that takes that key alone has its old orphan
/// deleted and a `cas/l/` key beside it kept; a store that takes another key refuses the sweep,
/// which is counted, and keeps both.
#[tokio::test]
async fn the_janitor_signs_with_the_key_file_against_the_pack_store() {
    let dir = scratch_dir("janitor-open");
    let s3 = FakeS3::start().await;
    let seeded = s3.store(Capabilities::default());
    let orphan = key(1, Slot::Pack(0));
    let large = key(1, Slot::Large(0));
    let content = format!("cas/l/{}", "cd".repeat(32));
    for name in [&orphan, &large, &content] {
        let name = ObjectKey::new(name.as_str()).expect("a key");
        seeded
            .put_new(&name, Bytes::from_static(b"x"), None)
            .await
            .expect("seeded");
    }
    let (pack_flags, packs) = packs_at(s3.endpoint(), dir.path());
    let flags = janitor_flags(dir.path());
    let open = || {
        let readiness = Arc::new(Readiness::default());
        flags.open(&pack_flags, &packs, index(), None, readiness)
    };

    s3.require_credentials("another-key", "another-secret");
    let wrong = open().expect("opens");
    wrong.sweep_once().await;
    assert_eq!(wrong.metrics().get("sweep_errors"), 1);
    assert_eq!(s3.keys().len(), 3);

    s3.require_credentials("pack-gc", "hunter2");
    let right = open().expect("opens");
    right.sweep_once().await;
    assert_eq!(right.metrics().get("reaped"), 2);
    assert_eq!(s3.keys(), [content]);
}

/// Asserts what opening refuses: a key file that is missing, and an endpoint that is no URL; and
/// that with no janitor flags nothing is opened, and with them but no packs or index it is an
/// error that names the flags.
#[tokio::test]
async fn opening_refuses_what_cannot_work() {
    let dir = scratch_dir("janitor-refuse");
    let s3 = FakeS3::start().await;
    let (pack_flags, packs) = packs_at(s3.endpoint(), dir.path());
    let readiness = Arc::new(Readiness::default());
    let mut flags = janitor_flags(dir.path());

    flags.key_file = dir.path().join("none.json");
    let error = flags.open(&pack_flags, &packs, index(), None, Arc::clone(&readiness));
    assert!(matches!(error, Err(JanitorFlagError::Key { .. })));

    let flags = janitor_flags(dir.path());
    let mut broken = pack_flags.clone();
    broken.endpoints = vec!["not a url".to_owned()];
    let error = flags.open(&broken, &packs, index(), None, Arc::clone(&readiness));
    let error = error.err().expect("refused").to_string();
    assert!(error.starts_with("--pack-store-endpoint "), "{error}");

    let index = index();
    let none = super::open(None, None, None, None, None, &readiness).expect("opens");
    assert!(none.is_none());
    let asked = Some(&flags);
    let error = super::open(asked, None, None, Some(&index), None, &readiness);
    let error = error.err().expect("refused").to_string();
    assert!(error.contains("--task-janitor-key-file needs"), "{error}");
    let state: Arc<dyn kbf_state::Store> = Arc::new(Faulty::new(Arc::default()));
    for state in [None, Some(&state)] {
        let opened = super::open(
            asked,
            Some(&pack_flags),
            Some(&packs),
            Some(&index),
            state,
            &readiness,
        );
        assert!(opened.expect("opens").is_some());
    }
}

/// A row of an open operation whose job commits into the put directory `dir`.
fn open_row(n: u64, dir: &str) -> (OperationId, OpRow) {
    let row = OpRow {
        owner: "fe".to_owned(),
        instance: "main".to_owned(),
        hash: "ab".repeat(32),
        size: 9,
        attempt: 0,
        job: format!("k0-{n}-0"),
        key: Some(dir.to_owned()),
        whole_machine: false,
        tree: None,
    };
    (OperationId(n), row)
}

/// Asserts the janitor the flags open over a store on loopback and the operations' rows of a
/// state store: an old key under `cas/b/` that no open operation names is counted by a dry run
/// (the default) and deleted with `--task-janitor-apply`, while an old key of a put directory an
/// open operation's row names is kept by both and counted, and a `cas/l/` key is never touched.
/// Catches: a janitor that deletes without `--task-janitor-apply`, or deletes a put directory
/// a queued or running job still commits into.
#[tokio::test]
async fn an_old_key_no_operation_names_is_counted_by_a_dry_run_and_deleted_with_apply() {
    let dir = scratch_dir("janitor-named");
    let s3 = FakeS3::start().await;
    let seeded = s3.store(Capabilities::default());
    let orphan = in_dir(2, 1, Slot::Pack(0));
    let named = in_dir(1, 1, Slot::Large(0));
    let content = format!("cas/l/{}", "cd".repeat(32));
    for name in [&orphan, &named, &content] {
        let name = ObjectKey::new(name.as_str()).expect("a key");
        let body = Bytes::from_static(b"x");
        seeded.put_new(&name, body, None).await.expect("seeded");
    }
    s3.require_credentials("pack-gc", "hunter2");
    let state = Arc::new(Faulty::new(Arc::default()));
    let rows = Arc::new(Rows::new(
        Arc::clone(&state) as Arc<dyn kbf_state::Store>,
        "fe",
    ));
    let (operation, row) = open_row(1, &dir_of(&named));
    rows.put_op(operation, &row).expect("a row");
    let (pack_flags, packs) = packs_at(s3.endpoint(), dir.path());
    let readiness = Arc::new(Readiness::default());
    let open = |apply: bool| {
        let flags = JanitorFlags {
            apply,
            ..janitor_flags(dir.path())
        };
        let rows = Some(Arc::clone(&rows));
        flags.open(&pack_flags, &packs, index(), rows, Arc::clone(&readiness))
    };

    let dry = open(false).expect("opens");
    dry.sweep_once().await;
    let counted = dry.metrics();
    assert_eq!(s3.keys().len(), 3, "a dry run deletes nothing");
    assert_eq!(counted.get("would_reap"), 1);
    assert_eq!(counted.get("kept_named"), 1);
    assert_eq!(counted.get("reaped"), 0);

    let applied = open(true).expect("opens");
    applied.sweep_once().await;
    let counted = applied.metrics();
    assert_eq!(counted.get("reaped"), 1);
    assert_eq!(counted.get("kept_named"), 1);
    let mut kept = vec![named.clone(), content.clone()];
    kept.sort();
    assert_eq!(s3.keys(), kept);

    state.reads_fail.store(true, Ordering::SeqCst);
    let failing = open(true).expect("opens");
    failing.sweep_once().await;
    assert_eq!(failing.metrics().get("sweep_errors"), 1);
    assert_eq!(s3.keys(), kept, "rows that cannot be read delete nothing");
}

/// Asserts the whole path from the flags: a frontend started with the janitor flags and
/// `--task-janitor-apply` over a store on loopback sweeps it as it serves, deleting the old orphan
/// pack and large object under `cas/b/` and leaving a `cas/l/` key, and counts it on
/// `GET /metrics`. The orphans carry the minute 1, so they are old at any time the sweep runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_the_janitor_flags_sweeps_the_store_as_it_serves() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("janitor-frontend");
    let seeded = s3.store(Capabilities::default());
    let content = format!("cas/l/{}", "ef".repeat(32));
    let names = [
        key(1, Slot::Pack(0)),
        key(1, Slot::Large(0)),
        content.clone(),
    ];
    for name in &names {
        let name = ObjectKey::new(name.as_str()).expect("a key");
        let body = Bytes::from_static(b"x");
        seeded.put_new(&name, body, None).await.expect("seeded");
    }
    let secret = file(
        dir.path(),
        "task.secret",
        "0123456789abcdef0123456789abcdef",
        0o600,
    );
    let gc = file(dir.path(), "gc.json", GC_KEY, 0o600);
    let mut more = pack_flags(s3.endpoint(), dir.path());
    more.extend([
        format!("--task-secret-file={}", secret.display()),
        "--task-out-root=/mnt/ts/put".to_owned(),
        format!("--task-janitor-key-file={}", gc.display()),
        "--task-janitor-interval-secs=1".to_owned(),
        "--task-janitor-apply".to_owned(),
    ]);
    let flags = flags_of(&more);
    assert!(flags.janitor.is_some());
    let handle = started(&flags, Some(index())).expect("starts");
    let api = handle.api.expect("the operator API");
    let mut sweeps = handle.janitor_sweeps().expect("a janitor");
    let served = tokio::spawn(handle.serve());

    // Each sweep that ends is told, so the scrape is repeated per sweep and not per time.
    let mut text = scrape(api).await;
    while !text.contains("kbf_cas_b_janitor_reaped_total 2\n") {
        sweeps.changed().await.expect("the janitor runs");
        text = scrape(api).await;
    }

    assert!(s3.keys().contains(&content), "{:?}", s3.keys());
    assert!(!s3.keys().contains(&names[0]));
    assert!(!s3.keys().contains(&names[1]));
    assert!(text.contains("kbf_cas_b_janitor_sweeps_total"), "{text}");
    served.abort();
}

/// Asserts that a frontend started with the janitor flags and without `--task-janitor-apply`
/// sweeps as it serves and deletes nothing: it counts the two old orphans it would delete, and
/// its gauge says the sweeps are dry runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_apply_counts_the_orphans_and_deletes_nothing() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("janitor-frontend-dry");
    let seeded = s3.store(Capabilities::default());
    let names = [key(1, Slot::Pack(0)), key(1, Slot::Large(0))];
    for name in &names {
        let name = ObjectKey::new(name.as_str()).expect("a key");
        let body = Bytes::from_static(b"x");
        seeded.put_new(&name, body, None).await.expect("seeded");
    }
    let secret = file(
        dir.path(),
        "task.secret",
        "0123456789abcdef0123456789abcdef",
        0o600,
    );
    let gc = file(dir.path(), "gc.json", GC_KEY, 0o600);
    let mut more = pack_flags(s3.endpoint(), dir.path());
    more.extend([
        format!("--task-secret-file={}", secret.display()),
        "--task-out-root=/mnt/ts/put".to_owned(),
        format!("--task-janitor-key-file={}", gc.display()),
    ]);
    let handle = started(&flags_of(&more), Some(index())).expect("starts");
    let api = handle.api.expect("the operator API");
    let mut sweeps = handle.janitor_sweeps().expect("a janitor");
    let served = tokio::spawn(handle.serve());

    // Each sweep that ends is told, so the scrape is repeated per sweep and not per time.
    let mut text = scrape(api).await;
    while !text.contains("\nkbf_cas_b_janitor_would_reap_total 2\n") {
        sweeps.changed().await.expect("the janitor runs");
        text = scrape(api).await;
    }

    assert!(text.contains("\nkbf_cas_b_janitor_apply 0\n"), "{text}");
    assert!(
        text.contains("\nkbf_cas_b_janitor_reaped_total 0\n"),
        "{text}"
    );
    let mut kept = names.to_vec();
    kept.sort();
    assert_eq!(s3.keys(), kept);
    served.abort();
}

/// Asserts that a frontend without the janitor flags exposes no janitor family and sweeps nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_the_janitor_flags_has_no_janitor() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("janitor-off");
    let flags = flags_of(&pack_flags(s3.endpoint(), dir.path()));
    assert_eq!(flags.janitor, None);
    let handle = started(&flags, Some(index())).expect("starts");
    let api = handle.api.expect("the operator API");
    let served = tokio::spawn(handle.serve());

    let text = scrape(api).await;

    assert!(!text.contains("kbf_cas_b_janitor_"), "{text}");
    served.abort();
}
