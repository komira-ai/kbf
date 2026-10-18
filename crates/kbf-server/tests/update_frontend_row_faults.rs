//! The `update/frontend` row over a store that holds wrong bytes or keeps racing, the
//! installer flags of the server, and `Installer::open`'s defaults.
//!
//! Each test names the defect it would catch.

#[path = "update_support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser as _;
use kbf_server::release::frontend_update::Installer;
use kbf_server::update::{Began, FrontendState, OrderId, Release, RowError, Rows};
use kbf_server::{Args, ConfigError};
use kbf_state::{Cond, Txn};
use support::harness::{Backing, Cell};
use support::stores::{Meddle, writes};

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];
const KEY: &str = "update/frontend";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The row's bytes: order id, epoch, generation, commit, state, written-at (tags 1 to 6).
fn row_bytes(generation: u8, state: u8) -> Vec<u8> {
    let mut bytes = vec![0x08, 1, 0x10, 7, 0x18, generation, 0x22, 40];
    bytes.extend_from_slice(COMMIT.as_bytes());
    bytes.extend_from_slice(&[0x28, state, 0x30, 1]);
    bytes
}

fn put_raw(cell: &Cell, bytes: &[u8]) {
    cell.store()
        .txn(Txn::new().put(KEY, bytes.to_vec(), Cond::Any))
        .expect("a write")
        .expect("no condition");
}

/// Catches a row read that takes bytes it should refuse: no message at all, a state that is
/// zero or unknown, no commit, generation 0 (which would let the next update draw 1 again and
/// rank below the update it follows). Each is `Corrupt` naming the key; the good bytes read.
///
/// Mutants planted: state 0 read as open; the generation check dropped; the commit check
/// dropped; an unknown state read as closed.
#[test]
fn a_frontend_row_that_is_not_a_row_is_corrupt() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "frontend-corrupt");
        let rows = Rows::new(cell.store());
        put_raw(&cell, &row_bytes(1, 1));
        assert_eq!(
            rows.frontend()
                .expect("good bytes")
                .value
                .expect("a row")
                .state,
            FrontendState::Open
        );
        let mut no_commit = vec![0x08, 1, 0x10, 7, 0x18, 1];
        no_commit.extend_from_slice(&[0x28, 1, 0x30, 1]);
        for (what, bytes) in [
            ("not a message", vec![0xff, 0xff, 0xff]),
            ("state zero", row_bytes(1, 0)),
            ("state unknown", row_bytes(1, 3)),
            ("generation zero", row_bytes(0, 1)),
            ("no commit", no_commit),
        ] {
            put_raw(&cell, &bytes);
            let Err(RowError::Corrupt { key, .. }) = rows.frontend() else {
                panic!("{backing:?} {what}: the row was read");
            };
            assert_eq!(key, KEY, "{what}");
        }
    }
}

/// Catches an open or an end that never stops while another writer keeps winning, or one that
/// writes over the winner: a rival rewrites the row before every write, so each gives up with
/// `Contended` (the rival's row stands) instead of looping or overwriting.
///
/// Mutants planted: the retry bound dropped (the test's own bound trips); the loop ending in
/// `Ok(false)` instead of `Contended`.
#[test]
fn opening_or_ending_the_row_under_a_rival_gives_up_contended() {
    for backing in BACKINGS {
        // Opening: the row is closed, and a rival closes it again at a higher generation
        // before every write, so the version moves under the claim.
        let cell = Cell::new(backing, "frontend-begin-contended");
        let inner = cell.store();
        put_raw(&cell, &row_bytes(1, 2));
        let calls = Arc::new(AtomicU64::new(0));
        let hook = {
            let (inner, calls) = (Arc::clone(&inner), Arc::clone(&calls));
            move |txn: &Txn| {
                if writes(txn, KEY) {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    assert!(n < 500, "the loop never gave up");
                    let generation = u8::try_from(n + 2).expect("few");
                    inner
                        .txn(Txn::new().put(KEY, row_bytes(generation, 2), Cond::Any))
                        .expect("a rival write")
                        .expect("no condition");
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let error = rows
            .begin_frontend(&Release::new(COMMIT), 7, 1)
            .unwrap_err();
        assert_eq!(error, RowError::Contended(KEY), "{backing:?}");
        assert!(calls.load(Ordering::SeqCst) >= 2, "{backing:?}: it retried");
        let held = Rows::new(Arc::clone(&inner))
            .frontend()
            .unwrap()
            .value
            .unwrap();
        assert_eq!(
            held.state,
            FrontendState::Closed,
            "{backing:?}: nothing of ours"
        );

        // Ending: the row is open for the order, and a rival rewrites the same open row
        // before every write.
        let cell = Cell::new(backing, "frontend-end-contended");
        let inner = cell.store();
        let Began::Opened(row) = Rows::new(cell.store())
            .begin_frontend(&Release::new(COMMIT), 7, 1)
            .expect("begin")
        else {
            panic!("opened");
        };
        let bytes = inner.get(KEY).unwrap().unwrap().value;
        let calls = Arc::new(AtomicU64::new(0));
        let seen: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let hook = {
            let (inner, calls, seen) = (Arc::clone(&inner), Arc::clone(&calls), Arc::clone(&seen));
            move |txn: &Txn| {
                if writes(txn, KEY) {
                    assert!(calls.fetch_add(1, Ordering::SeqCst) < 500, "never gave up");
                    *seen.lock().unwrap_or_else(PoisonError::into_inner) += 1;
                    inner
                        .txn(Txn::new().put(KEY, bytes.clone(), Cond::Any))
                        .expect("a rival write")
                        .expect("no condition");
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let error = rows.end_frontend(row.order).unwrap_err();
        assert_eq!(error, RowError::Contended(KEY), "{backing:?}");
        let held = Rows::new(inner).frontend().unwrap().value.unwrap();
        assert_eq!(held.state, FrontendState::Open, "{backing:?}");
        assert_eq!(held.order, OrderId::new(row.order.epoch, row.order.seq));
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kbf-server-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a test directory");
    dir
}

/// Catches the installer flags not reaching the API: `--installer-orders-dir` opens the spool
/// (made if missing) and the status file defaults to `status.json` beside it, while
/// `--installer-status-file` names another; a spool that cannot be made is refused naming the
/// flag.
///
/// Mutants planted: the status flag ignored; the default taken from the spool's own directory
/// instead of its parent; the error not naming the flag.
#[test]
fn the_installer_flags_open_the_spool_and_name_the_status_file() {
    let dir = scratch("installer-flags");
    let orders = dir.join("installer").join("orders");
    let api = |extra: &[&str]| {
        let mut argv = vec![
            "kbf-server",
            "--listen",
            "127.0.0.1:0",
            "--worker-listen",
            "127.0.0.1:0",
            "--api-listen",
            "127.0.0.1:0",
        ];
        argv.extend_from_slice(extra);
        Args::parse_from(argv).api()
    };
    let orders_flag = format!("--installer-orders-dir={}", orders.display());
    let Some(on) = api(&[orders_flag.as_str()])
        .expect("opens")
        .expect("an API")
        .installer
    else {
        panic!("the installer is on");
    };
    assert!(orders.is_dir(), "the spool was made");
    assert_eq!(on.status_file, dir.join("installer").join("status.json"));

    let other = dir.join("elsewhere.json");
    let status_flag = format!("--installer-status-file={}", other.display());
    let named = api(&[orders_flag.as_str(), status_flag.as_str()])
        .expect("opens")
        .expect("an API")
        .installer
        .expect("on");
    assert_eq!(named.status_file, other);

    let off = api(&[]).expect("opens").expect("an API");
    assert!(off.installer.is_none(), "no flag, no installer");

    let blocker = dir.join("a-file");
    std::fs::write(&blocker, b"x").expect("a file");
    let under = format!(
        "--installer-orders-dir={}",
        blocker.join("orders").display()
    );
    let Err(e) = api(&[under.as_str()]) else {
        panic!("a spool under a file is refused");
    };
    assert!(matches!(e, ConfigError::Installer { .. }), "{e:?}");
    assert!(e.to_string().starts_with("--installer-orders-dir "), "{e}");
}

/// Catches `Installer::open` defaults and its debug text: with no status file named the file
/// is `status.json` beside the spool's directory, a named one is kept as given, and the debug
/// text names the spool and the status file (an operator reading a log can tell which).
#[test]
fn installer_open_defaults_the_status_file_and_describes_itself() {
    let dir = scratch("installer-open");
    let installer = Installer::open(&dir.join("i").join("orders"), None).expect("opens");
    assert_eq!(installer.status_file, dir.join("i").join("status.json"));
    let text = format!("{installer:?}");
    assert!(text.starts_with("Installer {"), "{text}");
    assert!(
        text.contains("status_file") && text.contains("status.json"),
        "{text}"
    );
    assert!(text.contains("spool"), "{text}");
    let named = dir.join("x.json");
    let installer = Installer::open(&dir.join("j"), Some(&named)).expect("opens");
    assert_eq!(installer.status_file, named);
}
