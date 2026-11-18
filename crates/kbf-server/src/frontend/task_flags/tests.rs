//! The task flags: what they need, what they make, and what opening them checks.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
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

/// The two flags a put-directory frontend is given.
const PUT_DIR_ONLY: [&str; 2] = [
    "--task-secret-file=/etc/kbf/task.secret",
    "--task-out-root=/mnt/ts/put",
];

const LEASE: &str = "00000000000000010000000000000002";
const SECRET: &str = "0123456789abcdef0123456789abcdef";

/// The flags of a frontend given `more` after the base.
fn parsed(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let argv = BASE.iter().chain(more).copied();
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// The kind of error the flags are refused with.
fn refused(more: &[&str]) -> ErrorKind {
    parsed(more).expect_err("refused").kind()
}

/// The put-directory flags `more` after the secret and the root make.
fn put_dir(more: &[&str]) -> PutDirFlags {
    let mut flags = PUT_DIR_ONLY.to_vec();
    flags.extend_from_slice(more);
    parsed(&flags)
        .expect("parses")
        .put_dir
        .expect("put-dir flags")
}

/// Asserts that no run is prepared without `--task-secret-file`, and that the flags that have a
/// default, which the config template writes as live lines, change nothing and need nothing
/// when it is left out.
#[test]
fn the_task_flags_are_off_unless_the_secret_file_is_given() {
    assert_eq!(parsed(&[]).expect("parses").put_dir, None);
    let companions = [
        "--task-out-format=one-pass",
        "--task-url-slack-secs=5",
        "--task-put-retry-secs=90",
        "--task-manifest-writes=4",
        "--task-closure-max-digests=10",
        "--task-closure-max-bytes=1024",
        "--task-max-object-entries=7",
        "--task-max-object-manifest-bytes=8",
        "--task-object-bytes-per-entry=9",
    ];
    assert_eq!(parsed(&companions).expect("parses").put_dir, None);
}

/// Asserts what each flag needs, so that the preparation is never half configured: the switch
/// needs the root, the root needs the switch, and the mode `put-dir` needs the root.
#[test]
fn the_flags_need_each_other() {
    let missing = ErrorKind::MissingRequiredArgument;
    assert_eq!(
        refused(&[PUT_DIR_ONLY[0]]),
        missing,
        "the secret needs the root"
    );
    assert_eq!(
        refused(&[PUT_DIR_ONLY[1]]),
        missing,
        "the root needs the secret"
    );
    assert_eq!(
        refused(&["--task-out-mode=put-dir"]),
        missing,
        "put-dir needs its root"
    );
}

/// Asserts that the flags make the setting they say, and that the defaults are the frontend's:
/// a slack of 10 minutes, half a million digests and 32 GiB, and the limits of an object.
#[test]
fn the_flags_make_the_setting_and_the_defaults_are_the_constants() {
    let set = put_dir(&[]);
    assert_eq!(set.secret_file, PathBuf::from("/etc/kbf/task.secret"));
    assert_eq!(set.roots, PutDirRoots::new("/mnt/ts/put").expect("roots"));
    assert_eq!(set.url_slack, Duration::from_secs(DEFAULT_URL_SLACK_SECS));
    assert_eq!(set.put_retry, Duration::from_secs(DEFAULT_PUT_RETRY_SECS));
    assert_eq!(set.manifest_writes, kbf_front::action_tree::MANIFEST_WRITES);
    assert_eq!(set.max_digests, ClosureLimits::DEFAULT.max_digests);
    assert_eq!(set.max_bytes, ClosureLimits::DEFAULT.max_bytes);
    assert_eq!(set.max_object_entries, 1 << 20);
    assert_eq!(set.max_object_manifest_bytes, 4 << 20);
    assert_eq!(set.object_bytes_per_entry, 1024);
    assert_eq!(set.directory_cache, None, "Directories stay in memory");

    let set = put_dir(&[
        "--task-url-slack-secs=0",
        "--task-put-retry-secs=0",
        "--task-manifest-writes=1",
        "--task-closure-max-digests=1000",
        "--task-closure-max-bytes=1048576",
        "--task-max-object-entries=7",
        "--task-max-object-manifest-bytes=8",
        "--task-object-bytes-per-entry=9",
        "--task-directory-cache-dir=/var/cache/kbf/directories",
        "--task-directory-cache-bytes=4096",
    ]);
    assert_eq!(set.url_slack, Duration::ZERO);
    assert_eq!(set.put_retry, Duration::ZERO);
    assert_eq!(set.manifest_writes, 1);
    assert_eq!(set.max_digests, 1_000);
    assert_eq!(set.max_bytes, 1 << 20);
    assert_eq!(set.max_object_entries, 7);
    assert_eq!(set.max_object_manifest_bytes, 8);
    assert_eq!(set.object_bytes_per_entry, 9);
    assert_eq!(
        set.directory_cache,
        Some((PathBuf::from("/var/cache/kbf/directories"), 4096))
    );
}

/// Asserts that the two flags of the cache of Directories come together, so that it is never
/// half configured: each alone is refused as a missing argument, and a size of 0 is refused.
#[test]
fn the_directory_cache_flags_need_each_other_and_a_size() {
    let missing = ErrorKind::MissingRequiredArgument;
    for alone in [
        "--task-directory-cache-dir=/var/cache/kbf/directories",
        "--task-directory-cache-bytes=4096",
    ] {
        let mut given = PUT_DIR_ONLY.to_vec();
        given.push(alone);
        assert_eq!(refused(&given), missing, "{alone}");
    }
    let mut given = PUT_DIR_ONLY.to_vec();
    given.extend([
        "--task-directory-cache-dir=/var/cache/kbf/directories",
        "--task-directory-cache-bytes=0",
    ]);
    assert_eq!(refused(&given), ErrorKind::ValueValidation);
}

/// Asserts the bounds of the numeric flags: the slack is 0 to 14400 seconds, and the budgets and
/// limits at least 1.
#[test]
fn the_numeric_flags_are_bounded() {
    for bad in [
        "--task-url-slack-secs=14401",
        "--task-closure-max-digests=0",
        "--task-closure-max-bytes=0",
        "--task-max-object-entries=0",
        "--task-max-object-manifest-bytes=0",
        "--task-object-bytes-per-entry=0",
    ] {
        let mut given = PUT_DIR_ONLY.to_vec();
        given.push(bad);
        assert_eq!(refused(&given), ErrorKind::ValueValidation, "{bad}");
    }
    let most = put_dir(&["--task-url-slack-secs=14400"]);
    assert_eq!(most.url_slack, Duration::from_secs(14_400));
}

/// Asserts that the flags of the mode that was removed are refused, as a flag and as a `kbf.toml`
/// key, with the flag named and the reason: the write endpoint, the bucket, the credentials, the
/// read URL, the key-set sizes.
#[test]
fn the_flags_of_the_presigned_mode_are_refused_naming_the_release() {
    for gone in super::REMOVED {
        let flag = format!("--{gone}=x");
        let mut given = PUT_DIR_ONLY.to_vec();
        given.push(&flag);
        let error = parsed(&given).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::ValueValidation, "{gone}");
        let said = error.to_string();
        assert!(
            said.contains(&format!("invalid value 'x' for '--{gone} <VALUE>': the flag was removed in this release (kbf-server 0.")),
            "{said}"
        );
        assert!(said.contains("delete the flag"), "{said}");
    }
}

/// Asserts the output mode: a put directory under `--task-out-root` is the only mode, and the
/// default; `presigned`, which was removed, is refused with a message that says so and names the
/// build, whatever else is given; a mode without its root, a root without its mode or the switch,
/// a root that is no absolute path in normal form and a mode that is neither are refused.
#[test]
fn the_output_mode_is_a_put_dir_under_its_root_and_presigned_is_refused() {
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    assert_eq!(put_dir(&[]).roots, roots);
    assert_eq!(put_dir(&["--task-out-mode=put-dir"]).roots, roots);

    for given in [
        vec!["--task-out-mode=presigned"],
        vec![
            "--task-out-mode=presigned",
            "--task-secret-file=/etc/kbf/task.secret",
            "--task-out-root=/mnt/ts/put",
        ],
    ] {
        let error = parsed(&given).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{given:?}");
        let said = error.to_string();
        assert!(
            said.contains("--task-out-mode presigned was removed in this release (kbf-server 0."),
            "{said}"
        );
        assert!(said.contains("--task-out-mode put-dir"), "{said}");
        assert!(said.contains("--task-put-endpoint"), "{said}");
    }

    let relative = parsed(&[
        "--task-secret-file=/etc/kbf/task.secret",
        "--task-out-mode=put-dir",
        "--task-out-root=mnt/put",
    ])
    .expect_err("refused");
    assert_eq!(relative.kind(), ErrorKind::ArgumentConflict);
    assert!(
        relative
            .to_string()
            .contains("--task-out-root: \"mnt/put\" is not an absolute path"),
        "{relative}"
    );
    assert_eq!(refused(&["--task-out-mode=s3"]), ErrorKind::InvalidValue);
}

/// Asserts `--task-out-format`: `one-pass` is the one value, given or not; `packs`, the value of
/// the releases before, is refused.
#[test]
fn the_out_format_is_one_pass_and_packs_is_refused() {
    for more in [&[][..], &["--task-out-format=one-pass"]] {
        let mut flags = PUT_DIR_ONLY.to_vec();
        flags.extend_from_slice(more);
        assert!(
            parsed(&flags).expect("parses").put_dir.is_some(),
            "{more:?}"
        );
    }
    let mut flags = PUT_DIR_ONLY.to_vec();
    flags.push("--task-out-format=packs");
    assert_eq!(
        parsed(&flags).expect_err("packs is gone").kind(),
        ErrorKind::InvalidValue
    );
}

/// The file called `name` in `dir` of this text and mode.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
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

fn put_dir_flags(dir: &Path) -> PutDirFlags {
    PutDirFlags {
        secret_file: file(dir, "task.secret", &format!("{SECRET}\n"), 0o600),
        roots: PutDirRoots::new("/mnt/ts/put").expect("roots"),
        url_slack: Duration::from_secs(DEFAULT_URL_SLACK_SECS),
        put_retry: Duration::from_secs(DEFAULT_PUT_RETRY_SECS),
        manifest_writes: kbf_front::action_tree::MANIFEST_WRITES,
        max_digests: ClosureLimits::DEFAULT.max_digests,
        max_object_entries: 1 << 20,
        max_object_manifest_bytes: 4 << 20,
        object_bytes_per_entry: 1024,
        max_bytes: ClosureLimits::DEFAULT.max_bytes,
        directory_cache: None,
    }
}

/// Asserts that the flags open a preparer of put directories, with the mint of the secret (it
/// verifies the directory the preparer gives), and that no preparer is built without the flags; a
/// secret file that is missing, readable by others or under 32 bytes is refused.
#[tokio::test]
async fn the_put_dir_flags_open_a_preparer_of_put_dirs() {
    let dir = scratch_dir("task-put-dir-open");
    let (backend, action) = farm().await;
    assert!(
        super::open_put_dir(None, &backend)
            .expect("opens")
            .is_none()
    );
    let set = put_dir_flags(dir.path());
    let prep = super::open_put_dir(Some(&set), &backend)
        .expect("opens")
        .expect("a preparer");
    let now = UNIX_EPOCH + Duration::from_secs(1_790_000_040);
    let run = prep
        .prepare_put_dir(LEASE, &action, now, Duration::from_secs(60))
        .await
        .expect("run");
    let mint = PackKeyMint::new(SECRET.as_bytes()).expect("a mint");
    assert_eq!(mint.verify_put_dir(&run.dir, &action), Ok(now));
    assert_eq!(run.not_after, 1_790_000_040 + 60 + DEFAULT_URL_SLACK_SECS);

    let mut broken = put_dir_flags(dir.path());
    broken.secret_file = dir.path().join("none");
    assert!(super::open_put_dir(Some(&broken), &backend).is_err());
    broken.secret_file = file(dir.path(), "wide.secret", SECRET, 0o640);
    let said = broken
        .open(backend.clone())
        .expect_err("refused")
        .to_string();
    assert!(
        said.contains("wide.secret") && said.contains("0640"),
        "{said}"
    );
    assert!(!said.contains(SECRET), "{said}");
    let short = "too short\n";
    broken.secret_file = file(dir.path(), "short.secret", short, 0o600);
    let error = broken.open(backend).expect_err("refused");
    assert!(matches!(error, TaskFlagError::Mint(_)), "{error}");
    assert!(error.to_string().contains("--task-secret-file"), "{error}");
    assert!(!error.to_string().contains("too short"), "{error}");
}

/// Asserts that the preparation of put directories, as the executor asks for it
/// ([`crate::executor::RunPrepare`]), gives the directory and the closure the mint of the secret
/// verifies, mints the directory alone, and refuses a lease that is no build id as the caller's in
/// both.
#[tokio::test]
async fn the_put_dir_preparation_makes_directories() {
    use crate::executor::{PrepareError, RunPrepare};

    let dir = scratch_dir("task-put-dir-seam");
    let (backend, action) = farm().await;
    let prep = put_dir_flags(dir.path()).open(backend).expect("opened");
    let now = UNIX_EPOCH + Duration::from_secs(1_790_000_040);
    let wall = Duration::from_secs(60);
    let run = RunPrepare::prepare_put_dir(&prep, LEASE, &action, now, wall)
        .await
        .expect("prepared");
    assert!(
        run.dir.starts_with(&format!("cas/b/{LEASE}/")),
        "{}",
        run.dir
    );
    assert_eq!(run.not_after, 1_790_000_040 + 60 + DEFAULT_URL_SLACK_SECS);
    let bad = RunPrepare::prepare_put_dir(&prep, "a.b", &action, now, wall)
        .await
        .expect_err("refused");
    assert!(matches!(bad, PrepareError::Refused(_)), "{bad:?}");
    let minted = RunPrepare::mint_put_dir(&prep, LEASE, &action, now, wall)
        .await
        .expect("minted");
    assert!(
        minted.dir.starts_with(&format!("cas/b/{LEASE}/")),
        "{}",
        minted.dir
    );
    assert_eq!(minted.not_after, run.not_after);
    let bad_mint = RunPrepare::mint_put_dir(&prep, "a.b", &action, now, wall)
        .await
        .expect_err("refused");
    assert!(matches!(bad_mint, PrepareError::Refused(_)), "{bad_mint:?}");
    let source = crate::executor::RunSource::of_put_dir(&Arc::new(prep));
    assert!(
        source
            .mint
            .verify_put_dir(
                &kbf_front::IssuedPutDir::new(
                    LEASE,
                    &run.dir[run.dir.rfind('/').expect("a nonce") + 1..]
                )
                .expect("a dir"),
                &action
            )
            .is_ok()
    );
}

/// Asserts that a cache directory the flags name opens with the preparer, and that one that
/// cannot be opened (a path under a regular file) fails the start, naming the flag and the path
/// and not the secret.
#[tokio::test]
async fn the_directory_cache_dir_is_opened_with_the_preparer_and_a_bad_one_fails_the_start() {
    let dir = scratch_dir("task-directory-cache");
    let (backend, _) = farm().await;
    let mut set = put_dir_flags(dir.path());
    let cache = dir.path().join("directories");
    set.directory_cache = Some((cache, 1 << 20));
    set.open(backend.clone()).expect("opens");

    let plain = file(dir.path(), "plain", "not a directory", 0o600);
    let under = plain.join("directories");
    set.directory_cache = Some((under.clone(), 1 << 20));
    let error = set.open(backend).expect_err("refused");
    assert!(
        matches!(error, TaskFlagError::Directories { .. }),
        "{error}"
    );
    let said = error.to_string();
    assert!(
        said.starts_with(&format!("--task-directory-cache-dir {}: ", under.display())),
        "{said}"
    );
    assert!(!said.contains(SECRET), "{said}");
}

/// A store holding an Action whose input root is one Directory of one file, and the digests of
/// the Action and of that Directory.
async fn farm_with_a_tree() -> (Backend, kbf_types::Digest, kbf_types::Digest) {
    let store: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let put = |message: Vec<u8>| {
        let store = Arc::clone(&store);
        async move { store.put(Bytes::from(message)).await.expect("stored") }
    };
    let command = reapi::Command {
        arguments: vec!["true".to_owned()],
        ..reapi::Command::default()
    };
    let command = put(command.encode_to_vec()).await;
    let file = put(b"an input".to_vec()).await;
    let root = reapi::Directory {
        files: vec![reapi::FileNode {
            name: "input".to_owned(),
            digest: Some(kbf_front::digest_to_proto(&file)),
            ..reapi::FileNode::default()
        }],
        ..reapi::Directory::default()
    };
    let root = put(root.encode_to_vec()).await;
    let action = reapi::Action {
        command_digest: Some(kbf_front::digest_to_proto(&command)),
        input_root_digest: Some(kbf_front::digest_to_proto(&root)),
        ..reapi::Action::default()
    };
    let action = put(action.encode_to_vec()).await;
    (Backend::remote(store), action, root)
}

/// Asserts that the two cache flags, given on the command line, reach the preparation: a run
/// prepared through the flags as parsed keeps its input root's Directory in the directory
/// `--task-directory-cache-dir` names, and parsed flags naming a directory that cannot be made
/// (a path under a regular file) fail the start naming that flag.
#[tokio::test]
async fn the_parsed_cache_flags_keep_the_directories_of_a_run_on_the_disk() {
    use crate::executor::RunPrepare;

    let dir = scratch_dir("task-directory-cache-parsed");
    let secret = file(dir.path(), "task.secret", &format!("{SECRET}\n"), 0o600);
    let cache = dir.path().join("directories");
    let given = |cache: &Path| {
        let args = [
            format!("--task-secret-file={}", secret.display()),
            "--task-out-root=/mnt/ts/put".to_owned(),
            format!("--task-directory-cache-dir={}", cache.display()),
            "--task-directory-cache-bytes=1048576".to_owned(),
        ];
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        parsed(&args)
            .expect("parses")
            .put_dir
            .expect("put-dir flags")
    };
    let flags = given(&cache);
    assert_eq!(flags.directory_cache, Some((cache.clone(), 1 << 20)));
    let (backend, action, root) = farm_with_a_tree().await;
    let prep = flags.open(backend.clone()).expect("opened");
    let now = UNIX_EPOCH + Duration::from_secs(1_790_000_040);
    let run = RunPrepare::prepare_put_dir(&prep, LEASE, &action, now, Duration::from_secs(60))
        .await
        .expect("prepared");
    assert_eq!(run.kept.directory_count(), 1);
    // The cache is written by a task the preparation does not wait for.
    prep.detached_writes().settled().await;
    let kept: Vec<String> = std::fs::read_dir(&cache)
        .expect("the cache directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(kept, [format!("{}-{}", root.hash_hex(), root.size_bytes)]);

    let plain = file(dir.path(), "plain", "not a directory", 0o600);
    let error = given(&plain.join("directories"))
        .open(backend)
        .expect_err("refused");
    assert!(
        error.to_string().starts_with("--task-directory-cache-dir "),
        "{error}"
    );
}

/// Asserts that `--task-manifest-writes` takes 1 to 4096 and refuses 0 (no write could ever be
/// made) and 4097, by the value. Catches: a bound of zero that hangs every preparation.
#[test]
fn the_manifest_writes_are_one_to_4096() {
    assert_eq!(
        put_dir(&["--task-manifest-writes=4096"]).manifest_writes,
        4096
    );
    let bad = ErrorKind::ValueValidation;
    let mut zero = PUT_DIR_ONLY.to_vec();
    zero.push("--task-manifest-writes=0");
    assert_eq!(refused(&zero), bad);
    let mut over = PUT_DIR_ONLY.to_vec();
    over.push("--task-manifest-writes=4097");
    assert_eq!(refused(&over), bad);
}
