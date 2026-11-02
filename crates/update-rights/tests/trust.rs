//! Reading a file the host trusts.

mod support;

use std::fs;
use std::os::unix::fs::symlink;

use support::{Scratch, my_uid};
use update_rights::trust::{TrustError, check_dir, read_trusted};

/// Catches a file the trusted account owns and nobody else can write not being read.
#[test]
fn a_file_of_the_trusted_account_is_read() {
    let scratch = Scratch::new("trust-ok");
    for mode in [0o644, 0o600, 0o444, 0o755] {
        let file = scratch.file("f", "contents", mode);
        assert_eq!(
            read_trusted(&file, my_uid()).unwrap(),
            b"contents",
            "{mode:o}"
        );
    }
}

/// Catches a file of another owner being read, and says whose it is.
#[test]
fn a_file_of_another_account_is_not() {
    let scratch = Scratch::new("trust-owner");
    let file = scratch.file("f", "x", 0o644);
    match read_trusted(&file, my_uid() + 1).unwrap_err() {
        TrustError::Owner { found, wanted } => {
            assert_eq!((found, wanted), (my_uid(), my_uid() + 1))
        }
        other => panic!("{other:?}"),
    }
}

/// Catches a file that others can write being read, by group, by others, and by both.
#[test]
fn a_file_others_can_write_is_not_read() {
    let scratch = Scratch::new("trust-mode");
    for mode in [0o620, 0o602, 0o666, 0o775] {
        let file = scratch.file("f", "x", mode);
        match read_trusted(&file, my_uid()).unwrap_err() {
            TrustError::Writable { mode: found } => assert_eq!(found, mode, "{mode:o}"),
            other => panic!("{mode:o}: {other:?}"),
        }
    }
    let error = TrustError::Writable { mode: 0o666 };
    assert!(error.to_string().contains("666"));
}

/// Catches a link being followed, a directory read as a file, and a missing file reported as
/// anything but an I/O error that says so.
#[test]
fn a_link_a_directory_and_a_missing_file_are_not_read() {
    let scratch = Scratch::new("trust-kind");
    let file = scratch.file("f", "x", 0o644);
    let link = scratch.path().join("link");
    symlink(&file, &link).unwrap();
    assert!(matches!(
        read_trusted(&link, my_uid()),
        Err(TrustError::Io(_))
    ));
    assert!(matches!(
        read_trusted(scratch.path(), my_uid()),
        Err(TrustError::NotRegular)
    ));
    match read_trusted(&scratch.path().join("missing"), my_uid()).unwrap_err() {
        TrustError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("{other:?}"),
    }
    assert!(TrustError::NotRegular.to_string().contains("regular"));
}

/// Catches a pipe being waited on: opening a named pipe nobody writes must not block.
#[test]
fn a_named_pipe_is_refused_and_not_waited_for() {
    let scratch = Scratch::new("trust-pipe");
    let pipe = scratch.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .unwrap();
    assert!(status.success());
    fs::set_permissions(&pipe, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    assert!(matches!(
        read_trusted(&pipe, my_uid()),
        Err(TrustError::NotRegular)
    ));
}

/// Catches a directory that others can write, a file, a link and a missing path being trusted.
#[test]
fn a_directory_is_trusted_when_its_account_alone_can_write_it() {
    let scratch = Scratch::new("trust-dir");
    let dir = scratch.dir("d", 0o755);
    assert!(check_dir(&dir, my_uid()).is_ok());
    assert!(matches!(
        check_dir(&dir, my_uid() + 1),
        Err(TrustError::Owner { .. })
    ));
    for mode in [0o775, 0o757, 0o777] {
        let loose = scratch.dir("loose", mode);
        assert!(
            matches!(
                check_dir(&loose, my_uid()),
                Err(TrustError::Writable { .. })
            ),
            "{mode:o}"
        );
    }
    let file = scratch.file("f", "x", 0o644);
    assert!(matches!(
        check_dir(&file, my_uid()),
        Err(TrustError::NotDirectory)
    ));
    let link = scratch.path().join("link");
    symlink(&dir, &link).unwrap();
    assert!(matches!(
        check_dir(&link, my_uid()),
        Err(TrustError::NotDirectory)
    ));
    assert!(matches!(
        check_dir(&scratch.path().join("missing"), my_uid()),
        Err(TrustError::Io(_))
    ));
    assert!(TrustError::NotDirectory.to_string().contains("directory"));
}
