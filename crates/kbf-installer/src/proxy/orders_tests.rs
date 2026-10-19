//! The order spool on scratch directories: how ids follow the clock and the counter, what
//! the counter's durability buys when a write dies half way, two writers at once, the
//! limits, and the root half's own reader on every record written.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_update::MAX_RECORDS;
use kbf_update::record::{file_name, parse_file_name};

use super::api::traits::OrderWriter;
use super::model::{Locator, Verb};
use super::orders::{EPOCH, MAX_PENDING, OrderSpool, SpoolError};
use super::record::{Durable, Fsync};
use crate::CommitId;
use crate::helper::order;

const _: () = assert!(MAX_PENDING < MAX_RECORDS);

struct Dirs {
    orders: PathBuf,
    state: PathBuf,
}

fn dirs(name: &str) -> Dirs {
    let root = kbf_host::testkit::scratch(&format!("proxy-orders-{name}"));
    let state = root.join("state");
    fs::create_dir(&state).expect("state dir");
    Dirs {
        orders: root.join("orders"),
        state,
    }
}

fn commit(c: char) -> CommitId {
    CommitId::new(&c.to_string().repeat(40)).expect("a commit")
}

/// A wall clock the test sets, in milliseconds.
#[derive(Clone)]
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn at(ms: u64) -> Clock {
        Clock(Arc::new(AtomicU64::new(ms)))
    }

    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    fn reader(&self) -> Box<dyn Fn() -> u64 + Send + Sync> {
        let now = Arc::clone(&self.0);
        Box::new(move || now.load(Ordering::SeqCst))
    }
}

fn open_with(
    d: &Dirs,
    clock: &Clock,
    sync: Box<dyn Durable + Send + Sync>,
) -> Result<OrderSpool, SpoolError> {
    OrderSpool::open_with(&d.orders, &d.state, clock.reader(), sync)
}

fn open(d: &Dirs, clock: &Clock) -> OrderSpool {
    open_with(d, clock, Box::new(Fsync)).expect("open")
}

fn frontend(spool: &mut OrderSpool) -> Locator {
    spool
        .write(Verb::FrontendUpdate, &commit('a'), 900)
        .expect("written")
}

/// The ids of the record files in `orders/`, ascending.
fn ids(d: &Dirs) -> Vec<u64> {
    let mut ids: Vec<u64> = fs::read_dir(&d.orders)
        .expect("ls")
        .filter_map(|entry| {
            let entry = entry.expect("entry");
            let (_, id) = parse_file_name(entry.file_name().to_str()?)?;
            entry.path().is_file().then_some(id)
        })
        .collect();
    ids.sort_unstable();
    ids
}

fn counter(d: &Dirs) -> String {
    fs::read_to_string(d.state.join("orders.json")).expect("the counter")
}

fn counter_of(high_water: u64) -> String {
    format!("{{\"epoch\":1,\"high_water\":{high_water}}}")
}

/// Catches an id that is not the wall clock in milliseconds (the real clock is the one
/// path no other test uses), and a counter another user could read or change.
///
/// Mutants planted: the clock read in seconds (the id is below the lower bound); the
/// counter written with mode 0644 (the mode assertion fails).
#[test]
fn an_id_is_the_wall_clock_in_milliseconds_and_the_counter_is_private() {
    let d = dirs("wall");
    let now = || {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).expect("1970");
        u64::try_from(since.as_millis()).expect("fits")
    };
    let before = now();
    let mut spool = OrderSpool::open(&d.orders, &d.state).expect("open");
    let locator = frontend(&mut spool);
    let after = now();
    assert!(
        (before..=after).contains(&locator.order_id),
        "{} is not in {before}..={after}",
        locator.order_id
    );
    assert_eq!(locator.epoch, EPOCH);
    assert_eq!(locator.generation, locator.order_id);
    let mode = fs::metadata(d.state.join("orders.json"))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Catches ids that repeat inside one millisecond (the id taken from the clock alone), ids
/// that ignore the clock (the counter alone, so they restart from 1 and sort below what an
/// earlier root half acted on), and a counter that does not hold the last id.
///
/// Mutants planted: `id = wall` (7000 three times); `id = high_water + 1` (1, 2, 3).
#[test]
fn ids_follow_the_clock_and_never_repeat_inside_a_millisecond() {
    let d = dirs("follow");
    let clock = Clock::at(7000);
    let mut spool = open(&d, &clock);
    let first: Vec<u64> = (0..3).map(|_| frontend(&mut spool).order_id).collect();
    assert_eq!(first, [7000, 7001, 7002]);
    clock.set(9000);
    assert_eq!(frontend(&mut spool).order_id, 9000);
    assert_eq!(counter(&d), counter_of(9000));
    assert_eq!(ids(&d), [7000, 7001, 7002, 9000]);
}

/// Catches a clock that steps back (an NTP step, a restored VM) handing out an id below one
/// the root half has already acted on, which it drops without a word. At the high-water
/// mark exactly, the id is still one above it.
///
/// Mutants planted: `max` dropped from the id (4000 after 5000); `max` turned into `min`
/// (4000 after 5000).
#[test]
fn a_clock_stepped_back_still_increases() {
    let d = dirs("back");
    let clock = Clock::at(5000);
    let mut spool = open(&d, &clock);
    assert_eq!(frontend(&mut spool).order_id, 5000);
    clock.set(4000);
    assert_eq!(frontend(&mut spool).order_id, 5001);
    assert_eq!(frontend(&mut spool).order_id, 5002);
    clock.set(5002);
    assert_eq!(frontend(&mut spool).order_id, 5003);
}

/// Catches a restart that numbers again from the clock while the clock is behind the
/// counter, and a counter that is not read at open.
///
/// Mutant planted: the counter not read at open (the first id after the restart is 1000).
#[test]
fn ids_keep_growing_across_a_restart_with_the_clock_behind_the_counter() {
    let d = dirs("restart");
    let clock = Clock::at(5000);
    let mut spool = open(&d, &clock);
    assert_eq!(frontend(&mut spool).order_id, 5000);
    assert_eq!(frontend(&mut spool).order_id, 5001);
    drop(spool);
    clock.set(1000);
    let mut spool = open(&d, &clock);
    assert_eq!(frontend(&mut spool).order_id, 5002);
    assert_eq!(ids(&d), [5000, 5001, 5002]);
}

/// Catches the crash the module doc names: the counter durable and the record not written.
/// A directory with something in it stands where the next record belongs, so the final
/// rename fails after the counter is durable, which is what is on disk after a crash
/// between the two (a crash cannot be staged, its state can). The restart must not hand
/// the reserved id out again; the id is a gap.
///
/// Mutant planted: the record written before the counter (the failed write reserved
/// nothing, so the restart hands out 101).
#[test]
fn a_crash_between_the_counter_and_the_record_leaves_a_gap_and_no_id_is_reused() {
    let d = dirs("gap");
    let clock = Clock::at(100);
    let mut spool = open(&d, &clock);
    assert_eq!(frontend(&mut spool).order_id, 100);
    let squat = d.orders.join(file_name(EPOCH, 101));
    fs::create_dir_all(squat.join("held")).expect("squat");
    let err = spool
        .write(Verb::FrontendUpdate, &commit('b'), 900)
        .unwrap_err();
    assert!(err.0.contains("cannot be written"), "{err:?}");
    assert_eq!(counter(&d), counter_of(101), "reserved before the record");
    drop(spool);
    fs::remove_dir_all(&squat).expect("clear");
    let mut spool = open(&d, &clock);
    assert_eq!(
        frontend(&mut spool).order_id,
        102,
        "101 is a gap, not handed out again"
    );
    assert_eq!(ids(&d), [100, 102]);
}

/// Catches a record that is visible while the counter that numbers it is not durable: the
/// restart would hand the same id to the next order, and the root half drops it as a
/// replay. The temp file of the counter cannot be made, the write fails, no record exists,
/// and nothing was reserved.
///
/// Mutant planted: the record written before the counter (a record is in `orders/` after
/// the failed write).
#[test]
fn no_record_is_written_when_the_counter_cannot_be_made_durable() {
    let d = dirs("counter-first");
    let clock = Clock::at(100);
    let mut spool = open(&d, &clock);
    let before = counter(&d);
    let temp = d.state.join(".orders.json.tmp");
    fs::create_dir(&temp).expect("squat");
    let err = spool
        .write(Verb::FrontendUpdate, &commit('a'), 900)
        .unwrap_err();
    assert!(err.0.contains("order counter"), "{err:?}");
    assert_eq!(ids(&d), Vec::<u64>::new(), "no record without a counter");
    assert_eq!(counter(&d), before);
    fs::remove_dir(&temp).expect("clear");
    assert_eq!(frontend(&mut spool).order_id, 100, "nothing was reserved");
}

/// Reports the syncs of the counter with what was visible at that moment.
struct Spy {
    orders: PathBuf,
    state: PathBuf,
    log: Arc<Mutex<Vec<String>>>,
}

impl Spy {
    fn note(&self, what: &str) {
        let records = fs::read_dir(&self.orders)
            .expect("ls")
            .filter(|e| {
                let name = e.as_ref().expect("entry").file_name();
                name.to_str().and_then(parse_file_name).is_some()
            })
            .count();
        let counter = fs::read_to_string(self.state.join("orders.json")).ok();
        self.log
            .lock()
            .expect("log")
            .push(format!("{what}: {records} records, counter {counter:?}"));
    }
}

impl Durable for Spy {
    fn file(&self, _file: &File) -> io::Result<()> {
        self.note("file");
        Ok(())
    }

    fn dir(&self, dir: &Path) -> io::Result<()> {
        assert_eq!(dir, self.state, "the counter's directory is synced");
        self.note("dir");
        Ok(())
    }
}

/// Catches the order of the counter's own steps and of the counter against the record: the
/// file's data is synced before the rename, the directory after it, and when the directory
/// is synced no record is visible yet, so a crash at any point leaves the counter at or
/// ahead of every record. A sync dropped, or moved across the rename, changes the log.
///
/// Mutants planted: the record written before the counter (one record visible at the
/// file sync); the directory sync dropped; the file sync dropped; the rename moved after
/// the directory sync.
#[test]
fn the_counter_is_synced_before_the_record_is_visible() {
    let d = dirs("syncs");
    let log = Arc::new(Mutex::new(Vec::new()));
    let spy = Spy {
        orders: d.orders.clone(),
        state: d.state.clone(),
        log: Arc::clone(&log),
    };
    let mut spool = open_with(&d, &Clock::at(100), Box::new(spy)).expect("open");
    log.lock().expect("log").clear();
    frontend(&mut spool);
    let seen = log.lock().expect("log").clone();
    assert_eq!(
        seen,
        [
            format!("file: 0 records, counter {:?}", Some(counter_of(0))),
            format!("dir: 0 records, counter {:?}", Some(counter_of(100))),
        ]
    );
    assert_eq!(ids(&d), [100]);
}

/// Syncs that fail when the test says so, as a full or failing disk does.
struct Flaky {
    file: Arc<AtomicBool>,
    dir: Arc<AtomicBool>,
}

impl Durable for Flaky {
    fn file(&self, _file: &File) -> io::Result<()> {
        if self.file.load(Ordering::SeqCst) {
            return Err(io::Error::other("the disk failed"));
        }
        Ok(())
    }

    fn dir(&self, _dir: &Path) -> io::Result<()> {
        if self.dir.load(Ordering::SeqCst) {
            return Err(io::Error::other("the disk failed"));
        }
        Ok(())
    }
}

/// Catches a sync error of the counter being ignored (the record would be written under an
/// id the counter may not hold): whether the data sync or the directory sync fails, the
/// write fails, no record is visible, and once the disk recovers the same id is free.
///
/// Mutants planted: the result of the data sync discarded; the result of the directory
/// sync discarded (a record is written in each case).
#[test]
fn a_failed_sync_of_the_counter_stops_the_write_before_the_record() {
    let d = dirs("flaky");
    let (file, dir) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let sync = Flaky {
        file: Arc::clone(&file),
        dir: Arc::clone(&dir),
    };
    let mut spool = open_with(&d, &Clock::at(100), Box::new(sync)).expect("open");
    for failing in [&file, &dir] {
        failing.store(true, Ordering::SeqCst);
        let err = spool
            .write(Verb::FrontendUpdate, &commit('a'), 900)
            .unwrap_err();
        assert!(err.0.contains("the disk failed"), "{err:?}");
        assert_eq!(ids(&d), Vec::<u64>::new(), "no record after a failed sync");
        failing.store(false, Ordering::SeqCst);
    }
    assert_eq!(frontend(&mut spool).order_id, 100);
}

/// Catches an `orders/` that cannot be made being accepted: a file stands where it belongs.
/// No mutant: this covers the error path of the spool's open, which cannot be ignored
/// without a spool to hand on.
#[test]
fn an_orders_path_that_is_a_file_is_refused_at_open() {
    let d = dirs("orders-file");
    fs::write(&d.orders, b"not a directory").expect("a file");
    let err = open_with(&d, &Clock::at(100), Box::new(Fsync)).err();
    assert!(matches!(err, Some(SpoolError::Orders(_))), "{err:?}");
}

/// Real syncs, with a pause inside the data sync: the window in which two writers that
/// are not serialised both read the same high-water mark.
struct Slow;

impl Durable for Slow {
    fn file(&self, file: &File) -> io::Result<()> {
        thread::sleep(Duration::from_micros(300));
        Fsync.file(file)
    }

    fn dir(&self, dir: &Path) -> io::Result<()> {
        Fsync.dir(dir)
    }
}

/// Catches two writers (the update API's actor through `OrderWriter`, the swap's thread
/// through `confirm`) allocating the same id or leaving the counter below an id in use. The
/// clock is fixed, so with the writers serialised the ids are exactly 1000..1040: any
/// repeat is a missing file, and a counter written out of order is a counter below 1039.
///
/// Mutants planted, each red in 3 of 3 runs (a worker's write fails on the counter's shared
/// temp file, or an id repeats): the lock held only while the high-water mark is read, and
/// taken again to store the id; the lock held while the id is chosen and released before
/// the counter is written.
#[test]
fn two_writers_never_share_an_id_and_the_counter_ends_on_the_last() {
    let d = dirs("race");
    let clock = Clock::at(1000);
    let spool = open_with(&d, &clock, Box::new(Slow)).expect("open");
    let mut api = spool.clone();
    let swap = spool.clone();
    let from_api = thread::scope(|scope| {
        let api = scope.spawn(move || {
            (0..20)
                .map(|_| frontend(&mut api).order_id)
                .collect::<Vec<u64>>()
        });
        let swap = scope.spawn(move || {
            for _ in 0..20 {
                swap.confirm(&commit('c')).expect("confirmed");
            }
        });
        swap.join().expect("swap thread");
        api.join().expect("api thread")
    });
    assert!(
        from_api.windows(2).all(|w| w[0] < w[1]),
        "one writer's ids are not increasing: {from_api:?}"
    );
    assert_eq!(ids(&d), (1000..1040).collect::<Vec<u64>>());
    assert_eq!(counter(&d), counter_of(1039));
}

/// Catches a lock that a panic inside a write leaves poisoned for ever: the clock panics
/// once, while the lock is held, and the next write still works (nothing was reserved, so
/// the counter is as it was).
///
/// Mutant planted: `lock().unwrap()` in place of taking the guard back from the poison.
#[test]
fn a_panic_inside_a_write_does_not_stop_the_writes_after_it() {
    let d = dirs("poison");
    let armed = Arc::new(AtomicBool::new(true));
    let trip = Arc::clone(&armed);
    let clock: Box<dyn Fn() -> u64 + Send + Sync> = Box::new(move || {
        assert!(!trip.swap(false, Ordering::SeqCst), "the clock failed");
        100
    });
    let spool = OrderSpool::open_with(&d.orders, &d.state, clock, Box::new(Fsync)).expect("open");
    let mut doomed = spool.clone();
    let died = thread::spawn(move || doomed.write(Verb::FrontendUpdate, &commit('a'), 900)).join();
    assert!(died.is_err(), "the first write panics inside the lock");
    let mut spool = spool;
    assert_eq!(frontend(&mut spool).order_id, 100);
}

/// Catches a counter file that is gone (the state directory restored from before, or
/// cleared): the ids start again from the clock, which is ahead of the old ones, and the
/// open writes the counter anew.
///
/// Mutant planted: a missing file treated as an error (the open fails).
#[test]
fn a_lost_counter_starts_again_from_the_clock() {
    let d = dirs("lost");
    let clock = Clock::at(100);
    let mut spool = open(&d, &clock);
    assert_eq!(frontend(&mut spool).order_id, 100);
    assert_eq!(frontend(&mut spool).order_id, 101);
    drop(spool);
    fs::remove_file(d.state.join("orders.json")).expect("lose it");
    clock.set(500);
    let mut spool = open(&d, &clock);
    assert_eq!(counter(&d), counter_of(0));
    assert_eq!(frontend(&mut spool).order_id, 500);
    assert_eq!(ids(&d), [100, 101, 500]);
}

/// Catches a counter that is trusted when it should not be: one that is cut short, has a
/// key this build does not write or lacks one, belongs to another epoch (every id of this
/// spool would sort below what the root half acted on) or cannot be read at all; and a
/// state directory that is not there, which fails at open and not at the first update.
///
/// Mutants planted: `deny_unknown_fields` dropped (the extra key is accepted); the epoch
/// check dropped (epoch 2 is accepted); a read error treated as a missing file (the link
/// loop is replaced by a new counter and the open succeeds).
#[test]
fn a_counter_that_cannot_be_trusted_is_refused_at_open() {
    let d = dirs("untrusted");
    let clock = Clock::at(1);
    let counter_path = d.state.join("orders.json");
    for text in [
        "{\"epoch\":1,",
        "{\"epoch\":1,\"high_water\":5,\"extra\":1}",
        "{\"epoch\":1}",
    ] {
        fs::write(&counter_path, text).expect("seed");
        let err = open_with(&d, &clock, Box::new(Fsync)).err();
        assert!(
            matches!(err, Some(SpoolError::CounterCorrupt(_))),
            "{text}: {err:?}"
        );
    }
    fs::write(&counter_path, "{\"epoch\":2,\"high_water\":5}").expect("seed");
    let err = open_with(&d, &clock, Box::new(Fsync)).err();
    assert!(
        matches!(err, Some(SpoolError::CounterEpoch { found: 2 })),
        "{err:?}"
    );
    fs::remove_file(&counter_path).expect("clear");
    // A link to itself: reading it fails, and the error is not "not found".
    std::os::unix::fs::symlink("orders.json", &counter_path).expect("a link loop");
    let err = open_with(&d, &clock, Box::new(Fsync)).err();
    assert!(matches!(err, Some(SpoolError::Counter(_))), "{err:?}");
    let missing = Dirs {
        orders: d.orders.clone(),
        state: d.state.join("nowhere"),
    };
    let err = open_with(&missing, &clock, Box::new(Fsync)).err();
    assert!(matches!(err, Some(SpoolError::Counter(_))), "{err:?}");
}

/// Catches a counter at the largest id wrapping to zero: the next id would be one already
/// used. The write is refused, no record is written, the counter is as it was.
///
/// Mutant planted: `checked_add` replaced by `wrapping_add` (the write succeeds, with the
/// clock's id).
#[test]
fn the_largest_id_is_refused_and_not_wrapped() {
    let d = dirs("exhausted");
    let top = counter_of(u64::MAX);
    fs::write(d.state.join("orders.json"), &top).expect("seed");
    let mut spool = open(&d, &Clock::at(100));
    let err = spool
        .write(Verb::FrontendUpdate, &commit('a'), 900)
        .unwrap_err();
    assert!(err.0.contains("largest id"), "{err:?}");
    assert_eq!(ids(&d), Vec::<u64>::new());
    assert_eq!(counter(&d), top);
}

/// Catches a full `orders/` accepting one more record (the root half would delete the
/// newest unread), the limit off by one, files that are not records being counted, and a
/// refusal that reserves an id. The limit is [`MAX_PENDING`]; a record the root half has
/// taken makes room again. Both writers report it, and the reason names the count.
///
/// Mutants planted: the guard removed; `>=` turned into `>` (the 49th is written);
/// every file counted, not only records (the fill stops short).
#[test]
fn a_full_orders_dir_refuses_and_reserves_nothing() {
    let d = dirs("full");
    let clock = Clock::at(100);
    let mut spool = open(&d, &clock);
    fs::write(d.orders.join("node-state.json"), b"{}").expect("the node's own file");
    fs::write(d.orders.join(".partial-x.json"), b"{").expect("a partial");
    for _ in 0..MAX_PENDING {
        spool.confirm(&commit('a')).expect("room");
    }
    assert_eq!(ids(&d).len(), MAX_PENDING);
    let before = counter(&d);
    let err = spool
        .write(Verb::FrontendUpdate, &commit('b'), 900)
        .unwrap_err();
    assert!(
        err.0.contains(&format!("holds {MAX_PENDING} records")),
        "{err:?}"
    );
    for err in [
        spool.revert(&commit('b')).unwrap_err(),
        spool.confirm(&commit('b')).unwrap_err(),
    ] {
        assert!(
            err.0.contains(&format!("holds {MAX_PENDING} records")),
            "{err:?}"
        );
    }
    assert_eq!(counter(&d), before, "a refusal reserves nothing");
    fs::remove_file(d.orders.join(file_name(EPOCH, 100))).expect("the root half takes one");
    let next = frontend(&mut spool);
    assert_eq!(next.order_id, 100 + MAX_PENDING as u64);
}

/// Catches an `orders/` that cannot be listed being written to anyway, and a refusal that
/// reserves an id: the directory is gone, the write fails naming it, the counter is as it
/// was, and when the directory is back the same id is free.
///
/// Mutant planted: the listing error ignored (the count is taken as zero, the counter
/// moves and the record write then fails).
#[test]
fn an_orders_dir_that_cannot_be_listed_refuses_and_reserves_nothing() {
    let d = dirs("unlistable");
    let clock = Clock::at(100);
    let mut spool = open(&d, &clock);
    fs::remove_dir(&d.orders).expect("remove the empty directory");
    let err = spool
        .write(Verb::FrontendUpdate, &commit('a'), 900)
        .unwrap_err();
    assert!(err.0.contains("cannot be opened or listed"), "{err:?}");
    assert_eq!(counter(&d), counter_of(0));
    fs::create_dir(&d.orders).expect("back");
    assert_eq!(frontend(&mut spool).order_id, 100);
}

/// Catches an `activate` order being written: it would carry no components, which the root
/// half refuses for it, and the proxy has nothing to activate.
///
/// Mutant planted: `Activate` mapped to the activate record verb (a record is written).
#[test]
fn an_activate_is_never_written() {
    let d = dirs("activate");
    let mut spool = open(&d, &Clock::at(100));
    let err = spool.write(Verb::Activate, &commit('a'), 900).unwrap_err();
    assert!(err.0.contains("never writes an activate"), "{err:?}");
    assert_eq!(ids(&d), Vec::<u64>::new());
    assert_eq!(counter(&d), counter_of(0));
}

/// Catches a record the root half cannot read as the proxy meant it: every id, epoch,
/// generation, verb, commit and window comes back from the root half's own parser
/// (`helper::order::parse`) as it was written, under the file name its scan keys on, and
/// with no claims. The `frontend_update` is compared to its bytes instead: it is a locator
/// and nothing else, which the parser of this tree does not accept yet (it still asks a
/// `frontend_update` record for components). When it does, the record belongs in the
/// parsed table below.
///
/// Mutants planted: `generation` written as 0 (the parsed order differs); the confirm
/// written with the revert verb; the epoch written as 2 (the file is not under its key);
/// a `components` key written (the frontend_update bytes differ).
#[test]
fn what_the_root_half_reads_is_what_was_written() {
    let d = dirs("cross");
    let mut spool = open(&d, &Clock::at(4242));
    spool
        .write(Verb::FrontendUpdate, &commit('a'), 900)
        .expect("frontend_update");
    spool
        .write(Verb::Revert, &commit('b'), 900)
        .expect("revert, as the update API writes it");
    spool.confirm(&commit('c')).expect("confirm");
    spool
        .revert(&commit('d'))
        .expect("revert, as the swap writes it");
    let read = |id: u64| {
        let name = file_name(EPOCH, id);
        assert_eq!(order::key_of(&name), Some((EPOCH, id)));
        fs::read(d.orders.join(name)).expect("the record")
    };
    let want = |id, verb, c, window_s| order::Order {
        id,
        epoch: EPOCH,
        generation: id,
        verb,
        commit: commit(c),
        window_s,
        claims: None,
    };
    assert_eq!(
        String::from_utf8(read(4242)).expect("text"),
        format!(
            "{{\"order_id\":4242,\"epoch\":1,\"generation\":4242,\"verb\":\"frontend_update\",\"commit\":\"{}\",\"window_s\":900}}",
            commit('a').as_str()
        )
    );
    for (id, verb, c, window_s) in [
        (4243, order::Verb::Revert, 'b', 900),
        (4244, order::Verb::Confirm, 'c', 0),
        (4245, order::Verb::Revert, 'd', 0),
    ] {
        assert_eq!(
            order::parse(EPOCH, id, &read(id)),
            Ok(want(id, verb, c, window_s))
        );
    }
    assert_eq!(ids(&d), [4242, 4243, 4244, 4245]);
}

/// Catches `frontend_update` (the swap's write of a locator the API already issued)
/// misbehaving: a record that is there is left alone, one that is gone is written again
/// as it was, nothing takes a new id, a locator this spool did not issue (an id above the
/// counter, another verb, another epoch) is refused, and a full `orders/` refuses it.
///
/// Mutants planted: the `has` check dropped (the sentinel is overwritten); the guard on the
/// id dropped (101 is written); the guard on the verb dropped (the confirm locator is
/// written); the guard on the epoch dropped; the room check dropped from the rewrite.
#[test]
fn frontend_update_writes_a_missing_locator_once_and_refuses_one_not_issued() {
    let d = dirs("reissue");
    let mut spool = open(&d, &Clock::at(100));
    let issued = frontend(&mut spool);
    let path = d.orders.join(file_name(EPOCH, 100));
    let original = fs::read(&path).expect("the record");
    spool.frontend_update(&issued).expect("already there");
    assert_eq!(fs::read(&path).expect("the record"), original);
    fs::write(&path, b"sentinel").expect("mark it");
    spool.frontend_update(&issued).expect("already there");
    assert_eq!(fs::read(&path).expect("the record"), b"sentinel");
    fs::remove_file(&path).expect("the root half took it");
    spool.frontend_update(&issued).expect("written again");
    assert_eq!(fs::read(&path).expect("the record"), original);
    assert_eq!(counter(&d), counter_of(100), "no new id");
    for bad in [
        Locator {
            order_id: 101,
            ..issued.clone()
        },
        Locator {
            verb: Verb::Confirm,
            ..issued.clone()
        },
        Locator {
            epoch: 2,
            ..issued.clone()
        },
    ] {
        let err = spool.frontend_update(&bad).unwrap_err();
        assert!(
            err.0
                .contains("not a frontend_update locator this spool issued"),
            "{err:?}"
        );
    }
    assert_eq!(ids(&d), [100]);
    fs::remove_file(&path).expect("the root half took it");
    for _ in 0..MAX_PENDING {
        spool.confirm(&commit('a')).expect("room");
    }
    let err = spool.frontend_update(&issued).unwrap_err();
    assert!(
        err.0.contains(&format!("holds {MAX_PENDING} records")),
        "{err:?}"
    );
    assert!(!ids(&d).contains(&100));
    fs::remove_file(d.orders.join(file_name(EPOCH, 101))).expect("the root half takes one");
    spool.frontend_update(&issued).expect("room again");
    assert!(ids(&d).contains(&100));
}
