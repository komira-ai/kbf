//! The collector's flags, its two key files, what opening it refuses, and a frontend started
//! with the flags, which collects a store on loopback as it serves.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use std::future::pending;

use clap::error::ErrorKind;

use super::*;
use crate::frontend::pack_tests::{flags_of, index, pack_flags, scrape};
use crate::frontend::{self, FrontendFlags, Prepared, boxed};
use crate::test_dir::scratch_dir;

const BASE: [&str; 5] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--api-listen=127.0.0.1:0",
];

/// The flags the packs need.
const PACKS: [&str; 5] = [
    "--state-store=postgres:///kbf?host=/run/postgresql",
    "--pack-index",
    "--pack-store-endpoint=http://127.0.0.1:9000",
    "--pack-store-bucket=tiers",
    "--pack-store-credentials-file=/etc/kbf/pack.json",
];

const KEY: &str = "--executor-list-sweep-key-file=/etc/kbf/gc-l.json";
const SIDECAR_KEY: &str = "--executor-list-sweep-sidecar-key-file=/etc/kbf/gc-l2.json";

fn parsed(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let argv = BASE.iter().chain(more).copied();
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// The pack flags, then `sweep`.
fn with<'a>(sweep: &[&'a str]) -> Vec<&'a str> {
    PACKS.iter().chain(sweep).copied().collect()
}

fn refused(more: &[&str]) -> ErrorKind {
    parsed(more).expect_err("refused").kind()
}

/// Asserts that there is no collector without the list key file, whatever its companions say;
/// that the two key files need each other and apply needs them (clap); and that the key files
/// need the packs (the parse), naming the flag.
#[test]
fn the_collector_is_off_unless_its_key_files_are_given() {
    let interval = "--executor-list-sweep-interval-secs=60";
    assert_eq!(parsed(&with(&[interval])).expect("parses").list_sweep, None);
    assert_eq!(parsed(&with(&[])).expect("parses").list_sweep, None);
    for alone in [KEY, SIDECAR_KEY, "--executor-list-sweep-apply"] {
        assert_eq!(
            refused(&with(&[alone])),
            ErrorKind::MissingRequiredArgument,
            "{alone}"
        );
    }
    let said = parsed(&[KEY, SIDECAR_KEY]).expect_err("refused");
    assert_eq!(said.kind(), ErrorKind::ArgumentConflict);
    assert!(said.to_string().contains("--pack-store-endpoint"), "{said}");
}

/// Asserts that the flags make the setting they say, and that the defaults are a dry run every
/// hour.
#[test]
fn the_flags_make_the_setting_and_the_default_is_a_dry_run_every_hour() {
    let set = parsed(&with(&[KEY, SIDECAR_KEY])).expect("parses");
    let set = set.list_sweep.expect("collector flags");
    assert_eq!(
        set,
        ListSweepFlags {
            key_file: PathBuf::from("/etc/kbf/gc-l.json"),
            sidecar_key_file: PathBuf::from("/etc/kbf/gc-l2.json"),
            apply: false,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
        }
    );
    let every = [
        KEY,
        SIDECAR_KEY,
        "--executor-list-sweep-apply",
        "--executor-list-sweep-interval-secs=600",
    ];
    let set = parsed(&with(&every)).expect("parses").list_sweep;
    let set = set.expect("collector flags");
    assert!(set.apply);
    assert_eq!(set.interval, Duration::from_secs(600));
}

/// Asserts the interval's bounds: under a minute and over a day are refused, both edges taken.
#[test]
fn the_interval_holds_to_its_bounds() {
    for flag in [
        "--executor-list-sweep-interval-secs=59",
        "--executor-list-sweep-interval-secs=86401",
    ] {
        assert_eq!(
            refused(&with(&[KEY, SIDECAR_KEY, flag])),
            ErrorKind::ValueValidation,
            "{flag}"
        );
    }
    for flag in [
        "--executor-list-sweep-interval-secs=60",
        "--executor-list-sweep-interval-secs=86400",
    ] {
        assert!(parsed(&with(&[KEY, SIDECAR_KEY, flag])).is_ok(), "{flag}");
    }
}

/// The file called `name` in `dir` of this text and mode.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

/// A GC key file of `prefix`.
fn gc_key(prefix: &str) -> String {
    format!(
        r#"{{"access_key_id": "pack-gc", "secret_access_key": "hunter2", "prefix": "{prefix}"}}"#
    )
}

/// The collector flags with the two key files in `dir`, applying.
fn sweep_flags(dir: &Path) -> ListSweepFlags {
    ListSweepFlags {
        key_file: file(dir, "gc-l.json", &gc_key(LIST_PREFIX), 0o600),
        sidecar_key_file: file(dir, "gc-l2.json", &gc_key(SIDECAR_PREFIX), 0o600),
        apply: true,
        interval: Duration::from_secs(60),
    }
}

/// Pack flags for the S3 service at `endpoint`.
fn packs_at(endpoint: &str, dir: &Path) -> PackFlags {
    flags_of(&pack_flags(endpoint, dir))
        .packs
        .expect("pack flags")
}

/// What opening the collector of `flags` says, as text.
fn opened(flags: &ListSweepFlags, packs: &PackFlags, rig: &Rig) -> String {
    let rows = Arc::new(rig_rows(rig));
    let readiness = Arc::new(Readiness::default());
    match flags.open(packs, index(), rows, readiness) {
        Ok(_) => "opened".to_owned(),
        Err(error) => error.to_string(),
    }
}

fn rig_rows(rig: &Rig) -> Rows {
    Rows::new(Arc::clone(&rig.state) as Arc<dyn kbf_state::Store>, "fe")
}

/// Asserts what the key files must be: each the GC key of exactly its own prefix (`cas/l/` for
/// the lists, `cas/l2/` for the sidecars), and private; a refusal names the flag, the file and
/// the collector, and never the secret; and an endpoint that is no URL is refused.
#[tokio::test]
async fn each_key_file_is_the_gc_key_of_its_own_prefix_and_private() {
    let dir = scratch_dir("list-sweep-key");
    let rig = Rig::new().await;
    let packs = packs_at(rig.s3.endpoint(), dir.path());
    let flags = sweep_flags(dir.path());
    assert_eq!(opened(&flags, &packs, &rig), "opened");

    let swapped = ListSweepFlags {
        key_file: flags.sidecar_key_file.clone(),
        sidecar_key_file: flags.key_file.clone(),
        ..flags.clone()
    };
    let text = opened(&swapped, &packs, &rig);
    assert_eq!(
        text,
        format!(
            "--executor-list-sweep-key-file {}: the prefix is \"cas/l2/\" and must be exactly \
             \"cas/l/\": the collector of tree lists sweeps nothing else",
            flags.sidecar_key_file.display()
        )
    );
    for prefix in ["cas/b/", "cas/", "cas/l", ""] {
        let wrong = ListSweepFlags {
            sidecar_key_file: file(dir.path(), "other.json", &gc_key(prefix), 0o600),
            ..flags.clone()
        };
        let text = opened(&wrong, &packs, &rig);
        assert!(
            text.starts_with("--executor-list-sweep-sidecar-key-file "),
            "{prefix:?}: {text}"
        );
        assert!(text.contains("must be exactly \"cas/l2/\""), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }
    let open = ListSweepFlags {
        key_file: file(dir.path(), "open.json", &gc_key(LIST_PREFIX), 0o644),
        ..flags.clone()
    };
    let text = opened(&open, &packs, &rig);
    assert!(
        text.starts_with("--executor-list-sweep-key-file "),
        "{text}"
    );
    assert!(!text.contains("hunter2"), "{text}");
    let empty = ListSweepFlags {
        key_file: file(dir.path(), "empty.json", "", 0o600),
        ..flags.clone()
    };
    assert!(opened(&empty, &packs, &rig).contains("exactly access_key_id"));

    let mut broken = packs.clone();
    broken.endpoints = vec!["not a url".to_owned()];
    let text = opened(&flags, &broken, &rig);
    assert!(text.starts_with("--pack-store-endpoint "), "{text}");
}

/// Asserts that with no collector flags nothing is opened, and with them but no pack flags, no
/// pack index or no state store it is an error naming the flag.
#[tokio::test]
async fn opening_needs_the_packs_the_index_and_a_state_store() {
    let dir = scratch_dir("list-sweep-refuse");
    let rig = Rig::new().await;
    let packs = packs_at(rig.s3.endpoint(), dir.path());
    let flags = sweep_flags(dir.path());
    let readiness = Arc::new(Readiness::default());
    let (index, state) = (index(), Arc::clone(&rig.state) as Arc<dyn kbf_state::Store>);
    let none = super::super::open(None, None, None, None, &readiness).expect("opens");
    assert!(none.is_none());
    let asked = Some(&flags);
    for (packs, index, state) in [
        (None, Some(&index), Some(&state)),
        (Some(&packs), None, Some(&state)),
        (Some(&packs), Some(&index), None),
    ] {
        let error = super::super::open(asked, packs, index, state, &readiness);
        let Err(error) = error else {
            panic!("opened with a part missing");
        };
        let error = error.to_string();
        assert!(
            error.starts_with("--executor-list-sweep-key-file needs"),
            "{error}"
        );
    }
    let opened = super::super::open(asked, Some(&packs), Some(&index), Some(&state), &readiness);
    assert!(opened.expect("opens").is_some());
    assert_eq!(
        super::super::check(asked, None).expect_err("refused"),
        "--executor-list-sweep-key-file needs --pack-store-endpoint (and --pack-index and a \
         state store): the collector lists and deletes the tree lists in the pack store"
    );
    assert_eq!(super::super::check(asked, Some(&packs)), Ok(()));
}

/// Asserts the whole path from the flags: a frontend started with the collector's flags over a
/// state store and a store on loopback collects it as it serves, deleting an old list and its sidecar no row names and leaving a young one, and counts it
/// on `GET /metrics`; a frontend without the flags has no family of the collector.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_the_collector_flags_collects_the_store_as_it_serves() {
    let rig = Rig::new().await;
    let dir = scratch_dir("list-sweep-frontend");
    rig.list(1, 300, 1).await;
    rig.list(2, 300, 4_000_000_000).await;
    let flags = sweep_flags(dir.path());
    let mut more = pack_flags(rig.s3.endpoint(), dir.path());
    more.extend([
        format!(
            "--executor-list-sweep-key-file={}",
            flags.key_file.display()
        ),
        format!(
            "--executor-list-sweep-sidecar-key-file={}",
            flags.sidecar_key_file.display()
        ),
        "--executor-list-sweep-apply".to_owned(),
    ]);
    let frontend_flags = flags_of(&more);
    assert_eq!(
        frontend_flags.list_sweep,
        Some(ListSweepFlags {
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            ..flags
        })
    );
    let state = crate::StateStore {
        store: Arc::clone(&rig.state) as Arc<dyn kbf_state::Store>,
        members: crate::members::MembershipConfig::default(),
        history: None,
    };
    let shutdown = boxed(|| Ok(pending::<()>()));
    let prepared = Prepared::new(&frontend_flags).expect("prepared");
    let handle = prepared
        .start(Some(state), Some(index()), shutdown, None)
        .expect("starts");
    let api = handle.api.expect("the operator API");
    let mut passes = handle.list_sweeps().expect("a collector");
    let served = tokio::spawn(handle.serve());

    // The first pass starts as the frontend serves; each that ends is told.
    while *passes.borrow_and_update() == 0 {
        passes.changed().await.expect("the collector runs");
    }
    let text = scrape(api).await;

    let keys = rig.keys();
    for gone in [list_key(1), sidecar_key(1)] {
        assert!(!keys.contains(&gone), "{gone}: {keys:?}");
    }
    for kept in [list_key(2), sidecar_key(2)] {
        assert!(keys.contains(&kept), "{kept}: {keys:?}");
    }
    assert!(
        text.contains("kbf_tree_list_sweep_deleted_lists_total 1\n"),
        "{text}"
    );
    assert!(text.contains("kbf_tree_list_sweep_apply 1\n"), "{text}");
    served.abort();

    let plain = flags_of(&pack_flags(rig.s3.endpoint(), dir.path()));
    assert_eq!(plain.list_sweep, None);
}
