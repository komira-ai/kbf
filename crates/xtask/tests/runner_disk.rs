//! `runner-disk` (`xtask::runner_disk`): the room check before the coverage run.

use std::path::Path;
use std::process::Command;

use xtask::Error;
use xtask::runner_disk::{Args, Disk, probe, problems, run};

const GIB: u64 = 1 << 30;

fn disk(device: u64, gib: u64) -> Disk {
    Disk {
        device,
        free_bytes: gib * GIB,
    }
}

/// Catches a check that passes a short disk, one that is off by one at the edge (exactly the
/// need is enough, one byte less is not), and a message that does not say which disk is
/// short and by how much.
#[test]
fn a_disk_with_less_than_it_needs_is_refused_naming_the_disk() {
    let at = Path::new("/mnt");
    assert!(problems(&disk(1, 100), &disk(2, 64), at, 60, 64).is_empty());
    let mut short = disk(2, 64);
    short.free_bytes -= 1;
    let said = problems(&disk(1, 100), &short, at, 60, 64);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("/mnt has 63 GiB free"), "{said:?}");
    assert!(said[0].contains("needs 64 GiB"), "{said:?}");
    let said = problems(&disk(1, 59), &disk(2, 90), at, 60, 64);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("root disk has 59 GiB free"), "{said:?}");
    assert!(said[0].contains("need 60 GiB"), "{said:?}");
}

/// Catches a second disk that is the root disk under another name (a runner with no `/mnt`
/// disk would put the build on the disk that ran out), and a check that stops at the first
/// reason instead of listing all of them.
#[test]
fn a_second_disk_that_is_the_root_disk_is_refused_and_every_reason_is_listed() {
    let at = Path::new("/mnt");
    let said = problems(&disk(1, 1), &disk(1, 1), at, 60, 64);
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(said[0].contains("/mnt is not a separate disk"), "{said:?}");
    assert!(said.iter().all(|s| s.starts_with("runner out of disk:")));
}

/// Catches `probe` reading the wrong number: the free bytes of a real directory must be
/// nonzero and fit under the disk's size, and a path that does not exist is an error, not a
/// disk with nothing free.
#[test]
fn probe_reads_a_real_directory_and_refuses_a_missing_one() {
    let root = probe(Path::new("/")).expect("the root disk");
    assert!(root.free_bytes > 0);
    assert!(matches!(
        probe(Path::new("/nonexistent-runner-disk")),
        Err(Error::Spawn { .. })
    ));
}

fn args(disk: &str, disk_min: u64, root_min: u64) -> Args {
    Args {
        disk: disk.into(),
        disk_min_free_gb: disk_min,
        root: "/".into(),
        root_min_free_gb: root_min,
    }
}

/// Catches `run` passing a disk the check should refuse and refusing one it should pass.
/// `/proc` is a device of its own with nothing free: it passes a need of zero and fails any
/// other, and the root disk passes a need of zero and is the same device as itself.
#[test]
fn run_passes_a_separate_disk_with_enough_and_refuses_the_rest() {
    let lines = run(&args("/proc", 0, 0)).expect("a need of nothing is met");
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("disks: 0 GiB free on /proc (needs 0)"),
        "{lines:?}"
    );
    let Err(Error::Refused(said)) = run(&args("/proc", 1, 0)) else {
        panic!("a disk with nothing free passed a need of 1 GiB");
    };
    assert!(said[0].contains("/proc has 0 GiB free"), "{said:?}");
    let Err(Error::Refused(said)) = run(&args("/", 0, 0)) else {
        panic!("the root disk passed as a second disk");
    };
    assert!(said[0].contains("not a separate disk"), "{said:?}");
}

/// Catches the verb not being wired to the binary or its exit status: a refusal exits 1 with
/// the reason on stderr, a pass exits 0.
#[test]
fn the_binary_exits_1_with_the_reason_when_a_disk_is_short() {
    let xtask = env!("CARGO_BIN_EXE_xtask");
    let out = Command::new(xtask)
        .args(["runner-disk", "--disk", "/proc", "--disk-min-free-gb", "1"])
        .args(["--root-min-free-gb", "0"])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("runner out of disk"));
    let out = Command::new(xtask)
        .args(["runner-disk", "--disk", "/proc", "--disk-min-free-gb", "0"])
        .args(["--root-min-free-gb", "0"])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}
