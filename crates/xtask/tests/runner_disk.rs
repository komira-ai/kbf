//! `runner-disk` (`xtask::runner_disk`): the room check before the coverage run.

use std::path::Path;
use std::process::Command;

use xtask::Error;
use xtask::runner_disk::{Args, free_bytes, problem, run};

const GIB: u64 = 1 << 30;

/// Catches a check that passes a short disk, one that is off by one at the edge (exactly the
/// need is enough, one byte less is not), and a message that does not say which disk is short,
/// by how much, or what the run needs, which is all a reader of a failed job has to go on.
#[test]
fn a_disk_with_less_than_it_needs_is_refused_naming_the_disk_and_the_numbers() {
    let at = Path::new("/work");
    assert_eq!(problem(at, 80 * GIB, 80), None);
    let said = problem(at, 80 * GIB - 1, 80).expect("one byte short is refused");
    assert!(said.starts_with("runner out of disk:"), "{said}");
    assert!(said.contains("/work has 79 GiB free"), "{said}");
    assert!(said.contains("need 80 GiB"), "{said}");
}

/// Catches `free_bytes` reading the wrong number: a real directory has something free, and a
/// path that does not exist is an error, not a disk with nothing free.
#[test]
fn free_bytes_reads_a_real_directory_and_refuses_a_missing_one() {
    assert!(free_bytes(Path::new("/")).expect("the root disk") > 0);
    assert!(matches!(
        free_bytes(Path::new("/nonexistent-runner-disk")),
        Err(Error::Spawn { .. })
    ));
}

/// Catches `run` passing a disk the check should refuse and refusing one it should pass:
/// `/proc` has nothing free, so it passes a need of zero and fails any other.
#[test]
fn run_passes_a_need_that_is_met_and_refuses_one_that_is_not() {
    let args = |min_free_gb| Args {
        path: "/proc".into(),
        min_free_gb,
    };
    let lines = run(&args(0)).expect("a need of nothing is met");
    assert_eq!(lines, ["disk of /proc: 0 GiB free (needs 0)"]);
    let Err(Error::Refused(said)) = run(&args(1)) else {
        panic!("a disk with nothing free passed a need of 1 GiB");
    };
    assert!(said[0].contains("/proc has 0 GiB free"), "{said:?}");
}

/// Catches the verb not being wired to the binary or its exit status: a refusal exits 1 with
/// the reason on stderr, a pass exits 0.
#[test]
fn the_binary_exits_1_with_the_reason_when_the_disk_is_short() {
    let xtask = env!("CARGO_BIN_EXE_xtask");
    let run = |need: &str| {
        Command::new(xtask)
            .args(["runner-disk", "--path", "/proc", "--min-free-gb", need])
            .output()
            .expect("run xtask")
    };
    let out = run("1");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("runner out of disk"));
    assert_eq!(run("0").status.code(), Some(0));
}
