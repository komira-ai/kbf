use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use kbf_host::path::Root;
use kbf_host::testkit::ScratchHost;

use super::*;
use crate::layout::Platform;
use crate::state::Locked;
use crate::testkit::{A, B, c};

fn host(name: &str) -> (ScratchHost, Layout) {
    let exec = ScratchHost::new(&format!("record-release-{name}"));
    for sub in [
        "run",
        "var/lib/kbf-installer",
        "var/lib/kbf-installer/releases",
    ] {
        fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    let layout = Layout::new(
        Root::new(&exec.root).expect("absolute"),
        Platform::current(),
    );
    (exec, layout)
}

/// The user running the test: the owner of what it creates.
fn euid(exec: &ScratchHost) -> u32 {
    fs::metadata(&exec.root).expect("scratch root").uid()
}

/// The header of the executable this host runs, then padding.
fn header() -> Vec<u8> {
    let mut bytes = vec![0_u8; 64];
    match BinaryKind::wanted(Platform::current(), Arch::current()) {
        BinaryKind::Elf(arch) => {
            bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
            let machine: u16 = if arch == Arch::X86_64 { 0x3e } else { 0xb7 };
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        }
        BinaryKind::MachO(arch) => {
            bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
            let cpu: u32 = if arch == Arch::X86_64 {
                0x0100_0007
            } else {
                0x0100_000c
            };
            bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
        }
        BinaryKind::Other => unreachable!("a host runs a binary"),
    }
    bytes
}

/// A release directory as a hand placement leaves it: `kbf`, 0755, nothing else.
fn place(exec: &ScratchHost, commit: &str) -> std::path::PathBuf {
    let dir = exec.at(&format!("/var/lib/kbf-installer/releases/{commit}"));
    fs::create_dir_all(&dir).expect("dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("mode");
    let binary = dir.join("kbf");
    fs::write(&binary, header()).expect("kbf");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("mode");
    dir
}

fn run(
    exec: &mut ScratchHost,
    layout: &Layout,
    commit: &str,
    apply: bool,
) -> Result<(), RecordError> {
    record(exec, layout, euid(exec), &c(commit), apply, &mut Vec::new())
}

fn current(layout: &Layout) -> Option<CommitId> {
    Locked::acquire(layout)
        .expect("lock")
        .read()
        .expect("read")
        .and_then(|s| s.current)
}

/// Catches a host that stays unrecorded: after `--apply` the release is the host's `current`
/// (state and link), carries the marker the proxy requires, and the marker names the commit.
#[test]
fn a_hand_placed_release_becomes_current_and_gets_its_marker() {
    let (mut exec, layout) = host("apply");
    let dir = place(&exec, A);
    run(&mut exec, &layout, A, true).expect("recorded");
    assert_eq!(current(&layout), Some(c(A)));
    assert_eq!(
        fs::read_to_string(dir.join(VERIFIED)).expect("marker"),
        format!("{A}\n")
    );
    let link = exec.at("/var/lib/kbf-installer/releases/current");
    assert_eq!(
        fs::read_link(&link)
            .expect("current link")
            .file_name()
            .and_then(|n| n.to_str()),
        Some(A)
    );
}

/// Catches a dry run that writes: nothing changes, and it still checks the files.
#[test]
fn a_dry_run_changes_nothing_and_still_refuses_a_bad_release() {
    let (mut exec, layout) = host("dry");
    let dir = place(&exec, A);
    run(&mut exec, &layout, A, false).expect("dry run");
    assert!(!dir.join(VERIFIED).exists());
    assert_eq!(current(&layout), None);
    assert!(
        run(&mut exec, &layout, B, false).is_err(),
        "B was never placed"
    );
}

/// Catches a repeat that breaks: the same commit again is accepted and leaves the marker as
/// it was.
#[test]
fn recording_the_same_release_again_is_a_no_op() {
    let (mut exec, layout) = host("repeat");
    let dir = place(&exec, A);
    run(&mut exec, &layout, A, true).expect("first");
    let before = fs::metadata(dir.join(VERIFIED))
        .expect("marker")
        .modified()
        .expect("mtime");
    run(&mut exec, &layout, A, true).expect("again");
    let after = fs::metadata(dir.join(VERIFIED))
        .expect("marker")
        .modified()
        .expect("mtime");
    assert_eq!(before, after);
    assert_eq!(current(&layout), Some(c(A)));
}

/// Catches a verb that replaces a running release: another commit is refused once one is
/// current, and the refused release gets no marker.
#[test]
fn a_second_release_is_refused_and_gets_no_marker() {
    let (mut exec, layout) = host("second");
    place(&exec, A);
    let other = place(&exec, B);
    run(&mut exec, &layout, A, true).expect("first");
    let err = run(&mut exec, &layout, B, true).expect_err("refused");
    assert!(
        matches!(err, RecordError::Release(ReleaseError::NotFirst(_))),
        "{err}"
    );
    assert!(!other.join(VERIFIED).exists());
    assert_eq!(current(&layout), Some(c(A)));
}

/// Catches vouching for what cannot be a release: each defect is refused, with nothing
/// written (no marker, no state).
#[test]
fn files_that_are_not_a_release_are_refused() {
    type Spoil = fn(&std::path::Path);
    let cases: [(&str, Spoil, &str); 6] = [
        (
            "no-binary",
            |d| fs::remove_file(d.join("kbf")).expect("rm"),
            "cannot read",
        ),
        (
            "script",
            |d| fs::write(d.join("kbf"), b"not an executable\n").expect("script"),
            "not the executable this host runs",
        ),
        (
            "group-writable",
            |d| {
                fs::set_permissions(d.join("kbf"), fs::Permissions::from_mode(0o775)).expect("mode")
            },
            "writable by group or others",
        ),
        (
            "not-executable",
            |d| {
                fs::set_permissions(d.join("kbf"), fs::Permissions::from_mode(0o644)).expect("mode")
            },
            "not executable",
        ),
        (
            "dir-writable",
            |d| fs::set_permissions(d, fs::Permissions::from_mode(0o777)).expect("mode"),
            "writable by group or others",
        ),
        (
            "link",
            |d| {
                let real = d.join("real");
                fs::rename(d.join("kbf"), &real).expect("mv");
                symlink(&real, d.join("kbf")).expect("link");
            },
            "not a plain file",
        ),
    ];
    for (name, spoil, want) in cases {
        let (mut exec, layout) = host(name);
        let dir = place(&exec, A);
        spoil(&dir);
        let err = run(&mut exec, &layout, A, true).expect_err(name);
        assert!(err.to_string().contains(want), "{name}: {err}");
        assert!(!dir.join(VERIFIED).exists(), "{name}");
        assert_eq!(current(&layout), None, "{name}");
    }
}

/// Catches a file owned by someone else being vouched for.
#[test]
fn a_release_owned_by_another_user_is_refused() {
    let (mut exec, layout) = host("owner");
    place(&exec, A);
    let other = euid(&exec) + 1;
    let err =
        record(&mut exec, &layout, other, &c(A), true, &mut Vec::new()).expect_err("not ours");
    assert!(err.to_string().contains("not owned"), "{err}");
}

/// Catches vouching for a binary the frontend child cannot run. The child runs as the proxy's
/// user, not root, and the files are root's (the check requires it), so only the execute bit for
/// others lets it start: a root-owned 0700 `kbf`, or a 0700 release directory, is refused with
/// the `chmod` that fixes it, and the proxy's own `runnable` source then agrees on the 0755 one.
#[test]
fn a_binary_only_its_owner_can_execute_is_refused_with_the_fix() {
    type Spoil = fn(&std::path::Path);
    let cases: [(&str, Spoil); 3] = [
        ("owner-only-binary", |d| {
            fs::set_permissions(d.join("kbf"), fs::Permissions::from_mode(0o700)).expect("mode");
        }),
        ("group-only-binary", |d| {
            fs::set_permissions(d.join("kbf"), fs::Permissions::from_mode(0o750)).expect("mode");
        }),
        ("owner-only-dir", |d| {
            fs::set_permissions(d, fs::Permissions::from_mode(0o700)).expect("mode");
        }),
    ];
    for (name, spoil) in cases {
        let (mut exec, layout) = host(name);
        let dir = place(&exec, A);
        spoil(&dir);
        let err = run(&mut exec, &layout, A, true).expect_err(name);
        let text = err.to_string();
        assert!(
            text.contains("not executable by the frontend's user"),
            "{name}: {text}"
        );
        assert!(text.contains("fix: chmod 755"), "{name}: {text}");
        assert!(!dir.join(VERIFIED).exists(), "{name}");
        assert_eq!(current(&layout), None, "{name}");
    }
    let (mut exec, layout) = host("world-executable");
    let dir = place(&exec, A);
    run(&mut exec, &layout, A, true).expect("0755 is recorded");
    assert!(dir.join(VERIFIED).exists());
}
