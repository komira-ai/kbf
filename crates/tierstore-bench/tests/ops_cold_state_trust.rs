//! The cold store's heal status file is a trust input: a person or another host's writer keeps
//! it, and the run stops on a reading it cannot trust. These tests read real files through the
//! machine's own host.

mod ops_support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tierstore_bench::coldstate::{ColdStateSource, OpsColdState};
use tierstore_bench::preflight::host::SystemHost;
use tierstore_bench::role::Role;

use ops_support::*;

/// A source in file mode over the real file `path`, the clock at [`NOW_MS`].
fn source_of(name: &str, path: &Path) -> (kbf_testkit::dir::TestDir, OpsColdState) {
    let (dir, _host, config, ops) = base(name);
    let mut ops = file_mode(&ops);
    ops.cold_heal_status_file = Some(path.to_owned());
    let spec = config.store(Role::Cold).unwrap().clone();
    let source = OpsColdState::new(Arc::new(SystemHost), ops, spec, dir.path().to_owned())
        .with_max_age(Duration::from_secs(30))
        .with_clock(Arc::new(|| NOW_MS));
    (dir, source)
}

/// The directories that hold a source and its file, the source and the file's path.
type Fixture = (
    kbf_testkit::dir::TestDir,
    kbf_testkit::dir::TestDir,
    OpsColdState,
    PathBuf,
);

/// A status file at `mode` in a scratch directory, and a source over it.
fn file_at(name: &str, mode: u32, text: &str) -> Fixture {
    let scratch =
        kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("{name}-f"));
    let path = scratch.path().join("heal.json");
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    let (dir, source) = source_of(name, &path);
    (scratch, dir, source, path)
}

fn refused(source: &OpsColdState) -> String {
    let err = source.read().unwrap_err().to_string();
    assert!(err.starts_with("cold state: unreadable"), "{err}");
    err
}

/// Catches the trust checks refusing the writer's own file: a regular file of the user, mode
/// 0644 or 0600, reads.
#[test]
fn the_operators_own_regular_file_reads() {
    for mode in [0o644, 0o600] {
        let (_s, _d, source, _p) = file_at("tb-trust-ok", mode, &status_file(NOW_MS));
        assert!(source.read().unwrap().is_some(), "{mode:o}");
    }
}

/// Catches a status file followed through a symbolic link: the link is refused though its
/// target is a good file.
#[test]
fn a_symlink_is_refused() {
    let (_s, _d, _src, target) = file_at("tb-trust-link-target", 0o644, &status_file(NOW_MS));
    let link = target.with_file_name("link.json");
    symlink(&target, &link).unwrap();
    let (_dir, source) = source_of("tb-trust-link", &link);
    refused(&source);
}

/// Catches a file that anyone else than the user may write being believed: group- and
/// other-writable files are refused.
#[test]
fn a_file_others_can_write_is_refused() {
    for mode in [0o664, 0o646, 0o666] {
        let (_s, _d, source, _p) = file_at("tb-trust-mode", mode, &status_file(NOW_MS));
        refused(&source);
    }
}

/// Catches a status file of any size being read: one byte over 64 KiB is refused, exactly 64 KiB
/// of the same JSON reads.
#[test]
fn a_file_over_the_size_cap_is_refused() {
    let pad = |len: usize| {
        let mut text = status_file(NOW_MS);
        text.push_str(&" ".repeat(len - text.len()));
        text
    };
    let (_s, _d, source, _p) = file_at("tb-trust-big", 0o644, &pad(64 * 1024 + 1));
    refused(&source);
    let (_s, _d, source, _p) = file_at("tb-trust-cap", 0o644, &pad(64 * 1024));
    assert!(source.read().unwrap().is_some());
}

/// Catches a status file that is not a regular file being opened: a directory is refused, and
/// so is a FIFO, which must be refused without a read that waits for a writer. The FIFO holds
/// a good file's text and has a writer, so a reader that opens it gets that text (and one that
/// reads to the end waits); the read runs on a thread and the test gives up on it after a
/// generous wait, as a guard against a hang and not as a measure of time.
#[test]
fn a_directory_and_a_fifo_are_refused_without_waiting() {
    let scratch = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-trust-fifo-f");
    let (_dir, source) = source_of("tb-trust-dir", scratch.path());
    refused(&source);

    let fifo = scratch.path().join("heal.fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    let mut writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .unwrap();
    std::io::Write::write_all(&mut writer, status_file(NOW_MS).as_bytes()).unwrap();
    let (_dir, source) = source_of("tb-trust-fifo", &fifo);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(source.read().map(|state| state.is_some())).ok();
    });
    let answer = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the read waits on a FIFO");
    let err = answer.unwrap_err().to_string();
    assert!(err.starts_with("cold state: unreadable"), "{err}");
    drop(writer);
}
