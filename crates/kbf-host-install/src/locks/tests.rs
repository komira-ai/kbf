use std::fs;

use kbf_testkit::dir::TestDir;

use super::{flock_held, held, table_holds, wait_lockable, wait_lockable_paced};
use crate::pace::fake::Fake;

const TABLE: &str = "\
1: POSIX  ADVISORY  WRITE 812 08:01:131075 0 EOF
2: FLOCK  ADVISORY  WRITE 900 fd:00:42 0 EOF
3: -> POSIX  ADVISORY  WRITE 901 08:01:777 0 EOF
4: OFDLCK ADVISORY  READ  77 b3:2a:9 0 0
";

/// Catches a parser that misreads the device (hex) or the inode (decimal), or that takes
/// a waiter for a holder.
#[test]
fn the_table_is_read() {
    assert!(
        table_holds(TABLE, 8, 1, 131_075),
        "a POSIX lock, device 08:01 in hex"
    );
    assert!(table_holds(TABLE, 0xfd, 0, 42), "a FLOCK lock");
    assert!(
        table_holds(TABLE, 0xb3, 0x2a, 9),
        "an open-file-description lock"
    );
    assert!(!table_holds(TABLE, 8, 1, 777), "a waiter holds nothing");
    assert!(!table_holds(TABLE, 8, 2, 131_075), "another device");
    assert!(!table_holds(TABLE, 9, 1, 131_075), "another major");
    assert!(!table_holds(TABLE, 8, 1, 131_076), "another inode");
    assert!(
        !table_holds(TABLE, 8, 1, 0x131075),
        "the inode is decimal, not hex"
    );
    assert!(!table_holds("", 8, 1, 131_075));
    assert!(!table_holds(
        "garbage\n1: POSIX\n\n:::\nx: y z\n1: POSIX ADVISORY WRITE 1 a:b:c 0 0\n",
        8,
        1,
        1
    ));
    assert!(!table_holds("1: POSIX ADVISORY WRITE 1 1:2 0 0\n", 1, 2, 0));
}

/// Catches a probe that blocks or takes the lock: a real lock held through another open of
/// the same file is seen, and gone once it is released.
#[test]
fn a_real_lock_is_seen_and_released() {
    let dir = TestDir::under(std::env::temp_dir(), "locks-real");
    let path = dir.join("lock-frontend");
    let file = fs::File::create(&path).unwrap();
    let table = std::path::Path::new("/proc/locks");
    assert!(!held(table, &path));
    file.lock().unwrap();
    assert!(held(table, &path));
    file.unlock().unwrap();
    assert!(!held(table, &path));
}

/// A missing lock file or table is not a held lock.
#[test]
fn missing_files_are_not_held() {
    let dir = TestDir::under(std::env::temp_dir(), "locks-missing");
    let lock = dir.join("lock");
    fs::write(&lock, "").unwrap();
    assert!(!held(&dir.join("no-table"), &lock));
    let table = dir.join("table");
    fs::write(&table, TABLE).unwrap();
    assert!(!held(&table, &dir.join("no-lock")));
}

/// Catches a probe that cannot see an `flock`: the lock an install holds (exclusive, through
/// another open of the file) reads as held, and as free once it is released; the probe does
/// not leave a lock of its own behind that would turn the next install away.
#[test]
fn an_flock_is_seen_and_the_probe_leaves_none() {
    let dir = TestDir::under(std::env::temp_dir(), "locks-flock");
    let path = dir.join("install.lock");
    let holder = fs::File::create(&path).unwrap();
    assert!(!flock_held(&path), "nobody holds it");
    holder.lock().unwrap();
    assert!(flock_held(&path), "held");
    assert!(flock_held(&path), "and still held after being asked");
    holder.unlock().unwrap();
    assert!(!flock_held(&path), "released");
    wait_lockable(&path);
    let other = fs::File::open(&path).unwrap();
    other
        .try_lock()
        .expect("the probe left no lock behind: an install can take it");
}

/// A lock taken by the same process on another descriptor counts (`flock` locks belong to
/// the open file, not the process), and a shared lock does not stop an install, so it is not
/// "held" in the sense of the probe's own shared request.
#[test]
fn only_an_exclusive_flock_blocks_the_probe() {
    let dir = TestDir::under(std::env::temp_dir(), "locks-flock-shared");
    let path = dir.join("install.lock");
    let reader = fs::File::create(&path).unwrap();
    reader.lock_shared().unwrap();
    assert!(!flock_held(&path));
    drop(reader);
    assert!(!flock_held(&path));
}

/// A missing lock file is idle (no install has ever run); one that cannot be asked is not
/// proved idle: a link in its place (not followed) and a path under a file.
#[test]
fn an_unreadable_lock_is_held() {
    let dir = TestDir::under(std::env::temp_dir(), "locks-flock-odd");
    assert!(!flock_held(&dir.join("none")), "missing");
    let real = dir.join("real");
    fs::write(&real, "").unwrap();
    let link = dir.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(
        flock_held(&link),
        "a link is not followed, so not proved idle"
    );
    assert!(flock_held(&real.join("below")), "a path through a file");
}

/// The test helper waits for a holder to let go, and returns at once when there is none or
/// no file: it pauses between looks, gets the lock the look after the holder lets go, and
/// makes no pause when nothing holds it.
#[test]
fn wait_lockable_waits_for_a_holder() {
    let dir = TestDir::under(std::env::temp_dir(), "locks-wait");
    let path = dir.join("install.lock");
    let clock = Fake::new();
    wait_lockable_paced(&path, &clock.pace());
    assert!(clock.sleeps().is_empty(), "no file, nothing to wait for");

    let holder = fs::File::create(&path).unwrap();
    holder.lock().unwrap();
    // The holder lets go while the helper pauses for the third time.
    let holder = std::cell::RefCell::new(Some(holder));
    let clock = Fake::new();
    clock.on_sleep(move |n| {
        if n == 3 {
            holder.borrow_mut().take().unwrap().unlock().unwrap();
        }
    });
    wait_lockable_paced(&path, &clock.pace());
    assert_eq!(
        clock.sleeps(),
        [std::time::Duration::from_millis(10); 3],
        "three pauses, then the lock was free"
    );
    assert!(!flock_held(&path));

    let free = Fake::new();
    wait_lockable_paced(&path, &free.pace());
    assert!(
        free.sleeps().is_empty(),
        "a lock nobody holds is had at once"
    );
}

/// A lock that is never let go is looked at 500 times, 10 ms apart, and left for the caller's
/// own assertion: the helper gives up and returns.
#[test]
fn wait_lockable_gives_up_on_a_lock_that_stays() {
    let dir = TestDir::under(std::env::temp_dir(), "locks-wait-stays");
    let path = dir.join("install.lock");
    let holder = fs::File::create(&path).unwrap();
    holder.lock().unwrap();
    let clock = Fake::new();
    wait_lockable_paced(&path, &clock.pace());
    assert_eq!(clock.sleeps().len(), 500);
    assert!(flock_held(&path));
    drop(holder);
}
