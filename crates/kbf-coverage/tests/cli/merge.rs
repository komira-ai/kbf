//! `kbf-coverage merge`: the shards' tracefiles added into one the gate then reads.

use std::path::Path;

use super::{code, kbf_coverage, run, stderr, stdout, workspace};

/// A shard's tracefile for the fixture workspace `root`: crate `a` lists lines 1 to 100
/// and the shard hits those in `hits`; crate `b` lists 10 lines and every shard hits
/// all of them.
fn shard_text(root: &Path, hits: std::ops::RangeInclusive<u64>) -> String {
    let r = root.display();
    let mut out = format!("TN:\nSF:{r}/crates/a/src/lib.rs\n");
    for line in 1..=100u64 {
        let count = u64::from(hits.contains(&line));
        out.push_str(&format!("DA:{line},{count}\n"));
    }
    out.push_str(&format!(
        "LF:100\nLH:{}\nend_of_record\nSF:{r}/crates/b/src/main.rs\n",
        hits.clone().count()
    ));
    for line in 1..=10 {
        out.push_str(&format!("DA:{line},1\n"));
    }
    out.push_str("LF:10\nLH:10\nend_of_record\n");
    out
}

fn put(root: &Path, shard: &str, text: &str) {
    let dir = root.join("shards").join(shard);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("lcov.info"), text).expect("write");
}

fn merge(root: &Path, expect: &str) -> std::process::Output {
    kbf_coverage(
        root,
        &[
            "merge",
            "--dir",
            "shards",
            "--expect",
            expect,
            "--out",
            "merged.info",
        ],
    )
}

/// Catches: a merge that does not add (the gate then sees one shard's half of the
/// lines), a merged file the gate cannot read back, or a gate that still passes on a
/// single shard: each half alone is at 50% and fails, the two merged are at 100% and
/// pass.
#[test]
fn the_gate_passes_on_the_merge_and_fails_on_a_part() {
    let w = workspace("merge-e2e", (1, 1), (0, 0));
    put(&w, "1", &shard_text(&w, 1..=50));
    put(&w, "2", &shard_text(&w, 51..=100));
    std::fs::write(w.join("lcov.info"), shard_text(&w, 1..=50)).expect("write");
    let o = run(&w, "", &[]);
    assert_eq!(code(&o), Some(1), "{}", stdout(&o));
    assert!(
        stdout(&o).contains("BELOW THRESHOLD (lines)"),
        "{}",
        stdout(&o)
    );

    let o = merge(&w, "2");
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("2 tracefile(s) into merged.info"),
        "{}",
        stdout(&o)
    );
    std::fs::rename(w.join("merged.info"), w.join("lcov.info")).expect("rename");
    let o = run(&w, "", &[]);
    assert_eq!(code(&o), Some(0), "{}", stdout(&o));
    assert!(
        stdout(&o).contains("| a | 100/100 | 100.00 |"),
        "{}",
        stdout(&o)
    );
}

/// Catches: a shard that never arrived (or an extra one) leaving the total short without
/// an error, a missing directory, shards of different builds merged, and an output that
/// cannot be written reported as success; each exits 2 and names the cause. A
/// subdirectory without an `lcov.info` is not a shard and is not counted.
#[test]
fn a_merge_that_cannot_be_trusted_exits_2() {
    let w = workspace("merge-bad", (1, 1), (0, 0));
    put(&w, "1", &shard_text(&w, 1..=50));
    std::fs::create_dir_all(w.join("shards/empty")).expect("mkdir");
    let o = merge(&w, "2");
    assert_eq!(code(&o), Some(2), "{}", stdout(&o));
    assert!(
        stderr(&o).contains("holds 1 shard tracefile(s)"),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("expected 2"), "{}", stderr(&o));
    assert!(!w.join("merged.info").exists());

    put(
        &w,
        "2",
        &shard_text(&w, 51..=100).replace("DA:100,", "DA:101,"),
    );
    let o = merge(&w, "2");
    assert_eq!(code(&o), Some(2), "{}", stdout(&o));
    assert!(stderr(&o).contains("differs from"), "{}", stderr(&o));
    assert!(!w.join("merged.info").exists());

    put(&w, "2", &shard_text(&w, 51..=100));
    let o = kbf_coverage(
        &w,
        &[
            "merge",
            "--dir",
            "shards",
            "--expect",
            "2",
            "--out",
            "no/such/dir/merged.info",
        ],
    );
    assert_eq!(code(&o), Some(2), "{}", stdout(&o));
    assert!(
        stderr(&o).contains("write no/such/dir/merged.info"),
        "{}",
        stderr(&o)
    );

    let o = kbf_coverage(
        &w,
        &[
            "merge",
            "--dir",
            "nowhere",
            "--expect",
            "2",
            "--out",
            "merged.info",
        ],
    );
    assert_eq!(code(&o), Some(2), "{}", stdout(&o));
    assert!(stderr(&o).contains("read nowhere"), "{}", stderr(&o));
}
