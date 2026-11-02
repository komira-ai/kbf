//! The updater's own update: stage beside, switch, restart; a binary that exits at start is
//! replaced by the previous one.

mod support;

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_step_runner::selfupdate::{
    FollowOn, Layout, START_LIMIT_BURST, fallback_unit, follow_on, main_dropin,
};
use kbf_step_runner::{ArtifactError, Finished, Staged};
use support::Scratch;
use update_hook::{Class, Phase, Verdict};

fn binary(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    path
}

/// The updater binary of a release, in its own staged tree.
const UPDATER: &str = "bin/updater";

/// A release tree under `dir` whose updater binary is a copy of `from`, sealed.
fn release(dir: &Path, name: &str, from: &Path) -> Staged {
    let tree = dir.join(format!("release-{name}"));
    fs::create_dir_all(tree.join("bin")).unwrap();
    fs::copy(from, tree.join(UPDATER)).unwrap();
    Staged::seal(&tree).unwrap()
}

fn done(verdict: Verdict) -> Finished {
    Finished {
        verdict,
        reason: None,
    }
}

fn ok() -> Finished {
    done(Verdict::Succeeded {
        promotion_pending: false,
    })
}

/// Catches a staged binary that is not whole, not executable, or that moves a link: staging puts
/// the file in `versions/` with mode 0755 and changes neither `current` nor `previous`; a version
/// that is not a plain file name is refused.
#[test]
fn staging_adds_a_version_and_moves_no_link() {
    let s = Scratch::new("su-stage");
    let layout = Layout::new(&s.path().join("bin"));
    let src = binary(s.path(), "new", "#!new\n");
    layout.stage("1.2.3", &src).unwrap();
    let staged = layout.dir().join("versions/1.2.3");
    assert_eq!(fs::read_to_string(&staged).unwrap(), "#!new\n");
    assert_eq!(
        fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(layout.current().unwrap(), None);
    assert_eq!(layout.previous().unwrap(), None);
    assert!(!layout.dir().join("versions/.1.2.3.stage").exists());
    for bad in ["", ".hidden", "a/b", "a b", "../x", "é"] {
        let err = layout.stage(bad, &src).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
    }
    assert!(layout.stage("1.2.4", &s.path().join("absent")).is_err());
}

/// Catches a switch that loses the way back: `previous` becomes what `current` was, a switch to
/// the version already current changes nothing (so a repeat after a crash keeps the real previous
/// one), and a version that was never staged is refused.
#[test]
fn a_switch_remembers_the_previous_binary_and_is_idempotent() {
    let s = Scratch::new("su-switch");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    layout.switch("v1").unwrap();
    assert_eq!(
        (layout.current().unwrap(), layout.previous().unwrap()),
        (Some("v1".into()), None)
    );
    layout.switch("v2").unwrap();
    layout.switch("v2").unwrap();
    assert_eq!(
        (layout.current().unwrap(), layout.previous().unwrap()),
        (Some("v2".into()), Some("v1".into()))
    );
    assert_eq!(fs::read_to_string(layout.current_path()).unwrap(), "b");
    assert_eq!(fs::read_to_string(layout.previous_path()).unwrap(), "a");
    assert!(layout.switch("v3").is_err());
    assert_eq!(
        layout.switch("../v1").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

/// Catches a flip back that does nothing, or flips twice: with no previous binary it reports
/// `false`; after a switch it points `current` at the previous one once.
#[test]
fn flipping_back_restores_the_previous_binary_once() {
    let s = Scratch::new("su-flip");
    let layout = Layout::new(&s.path().join("bin"));
    assert!(!layout.flip_back().unwrap());
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    layout.switch("v1").unwrap();
    layout.switch("v2").unwrap();
    assert!(layout.flip_back().unwrap());
    assert_eq!(layout.current().unwrap(), Some("v1".to_owned()));
    assert!(!layout.flip_back().unwrap());
}

/// Catches a link move that trips on what a crash left: a temporary link from an earlier attempt
/// is replaced, and one that cannot be removed is an error rather than a silent skip; a `current`
/// that is not a link is an error too.
#[test]
fn leftovers_of_a_crash_are_replaced_and_odd_files_refused() {
    let s = Scratch::new("su-leftover");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    fs::write(layout.dir().join("current.tmp"), "stale").unwrap();
    layout.switch("v1").unwrap();
    assert_eq!(layout.current().unwrap(), Some("v1".to_owned()));

    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    fs::create_dir(layout.dir().join("previous.tmp")).unwrap();
    assert!(layout.switch("v2").is_err());

    let other = Layout::new(&s.path().join("odd"));
    fs::create_dir_all(other.dir()).unwrap();
    fs::write(other.current_path(), "not a link").unwrap();
    assert!(other.current().is_err());
}

/// Catches a follow-on that restarts when it should not or not when it should: only a succeeded
/// update that is not already this version stages, switches and asks for the restart; a repeat
/// after a crash gives the same state; a failed update, or the new binary started, does nothing.
#[test]
fn the_follow_on_restarts_only_after_a_success_that_changes_the_binary() {
    let s = Scratch::new("su-follow");
    let layout = Layout::new(&s.path().join("bin"));
    let v1 = binary(s.path(), "a", "a");
    let v2 = release(s.path(), "v2", &binary(s.path(), "b", "b"));
    let updater = Path::new(UPDATER);
    layout.stage("v1", &v1).unwrap();
    layout.switch("v1").unwrap();

    for verdict in [
        Verdict::Failed {
            phase: Phase::GateRelease,
            class: Class::Release,
        },
        Verdict::Deferred {
            retry_after_secs: None,
        },
        Verdict::Halt,
    ] {
        assert_eq!(
            follow_on(&done(verdict), &layout, "v1", "v2", &v2, updater).unwrap(),
            FollowOn::NotNeeded
        );
        assert_eq!(layout.current().unwrap(), Some("v1".to_owned()));
    }
    assert_eq!(
        follow_on(&ok(), &layout, "v2", "v2", &v2, updater).unwrap(),
        FollowOn::NotNeeded
    );
    assert!(!layout.dir().join("versions/v2").exists());

    assert_eq!(
        follow_on(&ok(), &layout, "v1", "v2", &v2, updater).unwrap(),
        FollowOn::Restart
    );
    assert_eq!(
        follow_on(&ok(), &layout, "v1", "v2", &v2, updater).unwrap(),
        FollowOn::Restart
    );
    assert_eq!(
        (layout.current().unwrap(), layout.previous().unwrap()),
        (Some("v2".into()), Some("v1".into()))
    );
    let pending = done(Verdict::Succeeded {
        promotion_pending: true,
    });
    assert_eq!(
        follow_on(&pending, &layout, "v2", "v2", &v2, updater).unwrap(),
        FollowOn::NotNeeded
    );
    assert!(follow_on(&ok(), &layout, "v2", "v3", &v2, Path::new("bin/absent")).is_err());
}

fn system_binary(name: &str) -> PathBuf {
    ["/bin", "/usr/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.exists())
        .expect("a system binary")
}

/// Catches a fallback that leaves the host with a broken updater: a binary that exits at
/// start fails `START_LIMIT_BURST` starts of the link the unit runs; the flip the fallback unit
/// makes puts the previous binary back, and the next start succeeds.
#[test]
fn a_binary_that_exits_at_start_is_replaced_by_the_previous() {
    let s = Scratch::new("su-exits-at-start");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("good", &system_binary("true")).unwrap();
    layout.switch("good").unwrap();
    let ran = Command::new(layout.current_path()).status().unwrap();
    assert!(ran.success());

    let new = release(s.path(), "bad", &system_binary("false"));
    assert_eq!(
        follow_on(&ok(), &layout, "good", "bad", &new, Path::new(UPDATER)).unwrap(),
        FollowOn::Restart
    );
    for _ in 0..START_LIMIT_BURST {
        let started = Command::new(layout.current_path()).status().unwrap();
        assert!(!started.success());
    }
    assert!(layout.flip_back().unwrap());
    assert!(
        Command::new(layout.current_path())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(layout.current().unwrap(), Some("good".to_owned()));
}

/// Catches the unit texts drifting from what the fallback needs: the drop-in restarts the unit and
/// names the fallback after the burst; the fallback runs the previous binary's verb, then starts
/// the main unit again.
#[test]
fn the_units_run_the_fallback_after_the_burst() {
    assert_eq!(
        main_dropin("updater-fallback.service", START_LIMIT_BURST),
        "[Unit]\nOnFailure=updater-fallback.service\nStartLimitIntervalSec=300\nStartLimitBurst=3\n\n[Service]\nRestart=always\nRestartSec=1\n"
    );
    let layout = Layout::new(Path::new("/usr/libexec/kbf-updater"));
    assert_eq!(
        fallback_unit(&layout, "kbf-updater.service"),
        "[Unit]\nDescription=Put the previous updater binary back after the new one failed to start\n\n[Service]\nType=oneshot\nExecStart=/usr/libexec/kbf-updater/previous self-update-fallback --dir /usr/libexec/kbf-updater\nExecStartPost=/usr/bin/systemctl reset-failed kbf-updater.service\nExecStartPost=/usr/bin/systemctl start --no-block kbf-updater.service\n"
    );
}

fn links(layout: &Layout) -> (Option<String>, Option<String>) {
    (layout.current().unwrap(), layout.previous().unwrap())
}

fn version_text(layout: &Layout, version: &str) -> String {
    fs::read_to_string(layout.dir().join("versions").join(version)).unwrap()
}

/// Catches a version the unit runs, or falls back to, being staged over (a crash between that
/// staging and the switch left the fallback replaced by a binary nothing had verified): staging
/// the version `current` or `previous` points at is refused and its file is left as it was; a
/// version neither names may be staged again.
#[test]
fn the_versions_current_and_previous_point_at_are_not_staged_over() {
    let s = Scratch::new("su-live");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    layout.switch("v1").unwrap();
    layout.switch("v2").unwrap();
    let other = binary(s.path(), "c", "c");
    for live in ["v1", "v2"] {
        let err = layout.stage(live, &other).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{live}");
        assert!(err.to_string().contains("current"), "{err}");
        assert!(
            !layout
                .dir()
                .join(format!("versions/.{live}.stage"))
                .exists()
        );
    }
    assert_eq!(version_text(&layout, "v1"), "a");
    assert_eq!(version_text(&layout, "v2"), "b");
    assert_eq!(links(&layout), (Some("v2".into()), Some("v1".into())));
    layout.stage("v3", &other).unwrap();
    layout.stage("v3", &binary(s.path(), "d", "d")).unwrap();
    assert_eq!(version_text(&layout, "v3"), "d");
}

/// Catches a flip back to a binary that is not there (the unit would then run nothing): with
/// `versions/<previous>` missing, `flip_back` is an error and `current` is not changed.
#[test]
fn flipping_back_to_a_missing_binary_is_refused() {
    let s = Scratch::new("su-flip-missing");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    layout.switch("v1").unwrap();
    layout.switch("v2").unwrap();
    fs::remove_file(layout.dir().join("versions/v1")).unwrap();
    let err = layout.flip_back().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(links(&layout), (Some("v2".into()), Some("v1".into())));
    assert_eq!(fs::read_to_string(layout.current_path()).unwrap(), "b");
}

/// Catches a follow-on that stages a file nothing verified: the binary is taken from the release
/// only through `Staged::check`, so a release changed since it was sealed, and a binary path
/// that leaves the tree, stage nothing and move no link.
#[test]
fn the_follow_on_stages_only_a_binary_of_the_verified_release() {
    let s = Scratch::new("su-follow-verified");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    layout.switch("v1").unwrap();

    let changed = release(s.path(), "v2", &binary(s.path(), "b", "b"));
    fs::write(changed.dir().join(UPDATER), "swapped").unwrap();
    let err = follow_on(&ok(), &layout, "v1", "v2", &changed, Path::new(UPDATER)).unwrap_err();
    assert!(matches!(err, ArtifactError::Changed { .. }), "{err:?}");

    let sound = release(s.path(), "v3", &binary(s.path(), "c", "c"));
    let outside = system_binary("true");
    for path in [Path::new("../release-v2/bin/updater"), outside.as_path()] {
        let err = follow_on(&ok(), &layout, "v1", "v3", &sound, path).unwrap_err();
        assert!(matches!(err, ArtifactError::BadHookPath(_)), "{err:?}");
    }
    assert!(!layout.dir().join("versions/v2").exists());
    assert!(!layout.dir().join("versions/v3").exists());
    assert_eq!(links(&layout), (Some("v1".into()), None));
}

/// Catches a follow-on to a version already in place copying over it, or never getting there: when
/// `current` or `previous` already names the version (a repeat after a crash, or a return to the
/// binary before), it is switched to without being staged again if its file holds the verified
/// binary's bytes, and refused with nothing changed if it does not.
#[test]
fn the_follow_on_to_a_version_in_place_stages_nothing() {
    let s = Scratch::new("su-follow-in-place");
    let layout = Layout::new(&s.path().join("bin"));
    let a = binary(s.path(), "a", "a");
    layout.stage("v1", &a).unwrap();
    layout.switch("v1").unwrap();
    let updater = Path::new(UPDATER);
    let v2 = release(s.path(), "v2", &binary(s.path(), "b", "b"));
    assert_eq!(
        follow_on(&ok(), &layout, "v1", "v2", &v2, updater).unwrap(),
        FollowOn::Restart
    );
    // Back to the binary before, which `previous` names.
    let v1 = release(s.path(), "v1", &a);
    assert_eq!(
        follow_on(&ok(), &layout, "v2", "v1", &v1, updater).unwrap(),
        FollowOn::Restart
    );
    assert_eq!(links(&layout), (Some("v1".into()), Some("v2".into())));
    assert_eq!(version_text(&layout, "v1"), "a");
    // A release that calls itself v2 with other bytes than the v2 in place.
    let not_v2 = release(s.path(), "not-v2", &binary(s.path(), "z", "z"));
    let err = follow_on(&ok(), &layout, "v1", "v2", &not_v2, updater).unwrap_err();
    assert!(
        matches!(&err, ArtifactError::Io(e) if e.kind() == io::ErrorKind::AlreadyExists),
        "{err:?}"
    );
    assert_eq!(version_text(&layout, "v2"), "b");
    assert_eq!(links(&layout), (Some("v1".into()), Some("v2".into())));
}

/// The file of a version, its inode and its bytes, to see that it was not written over.
fn file_of(layout: &Layout, version: &str) -> (u64, String) {
    use std::os::unix::fs::MetadataExt;
    let path = layout.dir().join("versions").join(version);
    let ino = fs::symlink_metadata(&path).unwrap().ino();
    (ino, fs::read_to_string(&path).unwrap())
}

/// Catches a staging copy made under a name a version can have: `v1.tmp` is a valid version,
/// and when it is the one `current` or `previous` points at, staging `v1` wrote its copy over
/// that file and renamed it away, leaving the link dangling. The copy is made under a name no
/// version has, so the live binary and both links stay as they were; a copy a crash left is
/// replaced, and a directory in its place is an error.
#[test]
fn staging_one_version_never_writes_over_another() {
    for live in ["current", "previous"] {
        let s = Scratch::new(&format!("su-stage-name-{live}"));
        let layout = Layout::new(&s.path().join("bin"));
        layout.stage("v1.tmp", &binary(s.path(), "a", "a")).unwrap();
        layout.stage("v0", &binary(s.path(), "z", "z")).unwrap();
        layout.switch("v1.tmp").unwrap();
        if live == "previous" {
            layout.switch("v0").unwrap();
        }
        let before = (links(&layout), file_of(&layout, "v1.tmp"));
        layout.stage("v1", &binary(s.path(), "b", "b")).unwrap();
        assert_eq!(
            (links(&layout), file_of(&layout, "v1.tmp")),
            before,
            "{live}"
        );
        assert_eq!(version_text(&layout, "v1"), "b");
        assert_eq!(fs::read_to_string(layout.dir().join(live)).unwrap(), "a");
    }

    let s = Scratch::new("su-stage-leftover");
    let layout = Layout::new(&s.path().join("bin"));
    fs::create_dir_all(layout.dir().join("versions")).unwrap();
    let leftover = layout.dir().join("versions/.v2.stage");
    fs::write(&leftover, "half a binary").unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    assert_eq!(version_text(&layout, "v2"), "b");
    assert!(!leftover.exists());
    fs::create_dir(layout.dir().join("versions/.v3.stage")).unwrap();
    assert!(layout.stage("v3", &binary(s.path(), "c", "c")).is_err());
    assert!(!layout.dir().join("versions/v3").exists());
}

/// Catches a fallback that is not a binary being taken for one: with `versions/<previous>` a
/// dangling link, a link to a binary, a directory or a file without an execute bit, `flip_back`
/// and `switch` are errors and change no link; and a missing fallback is an error even when
/// `current` already points at it (the fallback unit would otherwise report a flip it cannot
/// run as nothing to do).
#[test]
fn a_fallback_that_is_not_an_executable_file_is_refused() {
    let s = Scratch::new("su-flip-odd");
    let layout = Layout::new(&s.path().join("bin"));
    let a = binary(s.path(), "a", "a");
    layout.stage("v1", &a).unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    layout.switch("v1").unwrap();
    layout.switch("v2").unwrap();
    let v1 = layout.dir().join("versions/v1");
    let plain = s.path().join("plain");
    fs::write(&plain, "p").unwrap();
    type Make = Box<dyn Fn(&Path)>;
    let odd: [(&str, Make); 4] = [
        (
            "a dangling link",
            Box::new(|p: &Path| symlink("nowhere", p).unwrap()),
        ),
        (
            "a link to a binary",
            Box::new(move |p: &Path| symlink(&a, p).unwrap()),
        ),
        (
            "a directory",
            Box::new(|p: &Path| fs::create_dir(p).unwrap()),
        ),
        (
            "a file nobody may execute",
            Box::new(move |p: &Path| {
                fs::copy(&plain, p).unwrap();
                fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap();
            }),
        ),
    ];
    for (what, make) in odd {
        fs::remove_file(&v1)
            .or_else(|_| fs::remove_dir(&v1))
            .unwrap();
        make(&v1);
        let err = layout.flip_back().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{what}");
        assert_eq!(
            links(&layout),
            (Some("v2".into()), Some("v1".into())),
            "{what}"
        );
        assert!(layout.switch("v1").is_err(), "{what}");
        assert_eq!(
            links(&layout),
            (Some("v2".into()), Some("v1".into())),
            "{what}"
        );
    }

    // `current` and `previous` both name v3, whose file is gone.
    let s = Scratch::new("su-flip-same-missing");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v3", &binary(s.path(), "c", "c")).unwrap();
    layout.switch("v3").unwrap();
    symlink("versions/v3", layout.dir().join("previous")).unwrap();
    assert_eq!(links(&layout), (Some("v3".into()), Some("v3".into())));
    fs::remove_file(layout.dir().join("versions/v3")).unwrap();
    let err = layout.flip_back().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(links(&layout), (Some("v3".into()), Some("v3".into())));
}
