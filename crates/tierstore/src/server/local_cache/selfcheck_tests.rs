//! The start-up check of the cache directory.

use super::selfcheck::{LocalTierFailure, check_local};
use super::testing::{FakeDisk, Quirk, Rig};
use super::{SystemDisk, layout::stage_dir};

/// The check of `rig`'s directory on a file system with this quirk, as the text of its answer.
fn answer(rig: &Rig, quirk: Quirk) -> String {
    let disk = FakeDisk::new();
    disk.set(quirk);
    match check_local(&disk, &rig.config.path, "run") {
        Ok(()) => "passed".to_owned(),
        Err(LocalTierFailure::Failed { check, .. }) => check.to_owned(),
        Err(unusable) => unusable.to_string(),
    }
}

/// Asserts that the file system of the machine passes the four checks and leaves nothing behind,
/// and that a file system that answers one of them wrongly is refused naming that check: a
/// second link that overwrites, a name that shows other bytes than the file it was linked to,
/// bytes that read back changed, a link count that does not count.
#[tokio::test]
async fn a_file_system_that_answers_a_question_wrongly_is_refused_naming_it() {
    let rig = Rig::new("tscache-selfcheck");
    assert_eq!(answer(&rig, Quirk::None), "passed");
    let stage = stage_dir(&rig.config.path);
    assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0);
    let wrong = [
        (Quirk::LinkOverwrites, "create-only"),
        (Quirk::SecondDiffers, "atomic-visibility"),
        (Quirk::FirstDiffers, "raw-bytes"),
        (Quirk::OneLink, "st_nlink"),
    ];
    for (quirk, check) in wrong {
        assert_eq!(answer(&rig, quirk), check, "{quirk:?}");
        assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0, "{quirk:?}");
    }
}

/// Asserts that a directory that cannot hold the probe, and a file system whose link fails with
/// an error other than that the name exists, are refused as unusable, with the path in the text;
/// and that the texts of both refusals say what the start did.
#[tokio::test]
async fn a_directory_the_probe_cannot_use_is_refused_with_its_path() {
    let rig = Rig::new("tscache-selfcheck-unusable");
    let text = answer(&rig, Quirk::LinkFails(std::io::ErrorKind::PermissionDenied));
    assert!(
        text.starts_with("refusing to start: the local cache at "),
        "{text}"
    );
    assert!(
        text.contains(&rig.config.path.display().to_string()),
        "{text}"
    );

    let blocked = rig.dir.path().join("blocked");
    std::fs::write(&blocked, b"a file").unwrap();
    let error = check_local(&SystemDisk, &blocked, "run").unwrap_err();
    let text = error.to_string();
    assert!(
        text.starts_with("refusing to start: the local cache at "),
        "{text}"
    );
    assert!(text.contains(&blocked.display().to_string()), "{text}");
    let refused = LocalTierFailure::Failed {
        path: "/c".to_owned(),
        check: "create-only",
    };
    let text = refused.to_string();
    assert_eq!(
        text,
        "refusing to start: the file system of the local cache at /c failed the create-only check"
    );
}
