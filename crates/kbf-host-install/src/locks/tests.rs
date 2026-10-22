use std::fs;

use kbf_testkit::dir::TestDir;

use super::{held, table_holds};

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
