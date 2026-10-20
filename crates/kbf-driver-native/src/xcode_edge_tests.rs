//! Tests of [`super`] that answer at once: the edges of the Xcode survey and of the
//! `xcrun` warm-up that the slow tests of `xcode_tests.rs` also reach, but which the
//! fast gate must reach too.

use super::tests::{act_fake, scratch};
use super::*;

/// Catches: an app that merely starts with `Xcode` (a note, a download) or only ends
/// in `.app` (Safari) taken for an Xcode, an Xcode left out, or the apps not in name
/// order.
#[test]
fn only_xcode_apps_are_found_and_in_name_order() {
    let dir = scratch("edge-found");
    for name in [
        "Xcode_beta.app",
        "Xcode.app",
        "Xcode_notes.txt",
        "Xcode.dmg",
        "Safari.app",
        "xcode_lower.app",
    ] {
        std::fs::create_dir_all(dir.join(name)).expect("app");
    }
    let found = found_in(&dir);
    let names: Vec<_> = found
        .iter()
        .map(|p| p.strip_prefix(&*dir).expect("inside"))
        .collect();
    assert_eq!(names, [Path::new("Xcode.app"), Path::new("Xcode_beta.app")]);
    let unasked = not_surveyed(&dir);
    assert_eq!(
        unasked.iter().map(|x| x.state).collect::<Vec<_>>(),
        [State::NotSurveyed, State::NotSurveyed]
    );
}

/// Catches: a directory that is missing, or is a file, reported as an error or as
/// holding an Xcode instead of "no Xcode".
#[test]
fn a_directory_that_cannot_be_read_holds_no_xcode() {
    let dir = scratch("edge-unreadable");
    assert!(found_in(&dir.join("missing")).is_empty());
    let file = dir.join("Xcode.app");
    std::fs::write(&file, "not a directory").expect("file");
    assert!(found_in(&file).is_empty());
    assert!(not_surveyed(&dir.join("missing")).is_empty());
}

/// Catches: an `xcodebuild` path that leads out of the Xcode (absolute, or through
/// `..`) run, or an Xcode asked about it left `Ready` rather than `Failed` with the
/// path in the reason.
#[test]
fn an_xcodebuild_path_outside_the_xcode_fails_the_xcode_without_running_it() {
    let dir = scratch("edge-outside");
    let ran = dir.join("ran");
    let outside = act_fake(
        &dir.join("xcodebuild"),
        &format!("*\t-\tappend {} ran\n", ran.display()),
    );
    std::fs::create_dir_all(dir.join("Applications/Xcode_x.app")).expect("app");
    for path in [outside, PathBuf::from("../../../../xcodebuild")] {
        let probe = Probe {
            xcodebuild: path.clone(),
            ..Probe::system(false)
        };
        let found = survey(&dir.join("Applications"), &probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, State::Failed);
        assert_eq!(
            found[0].reason,
            format!("{} is not inside the Xcode", path.display())
        );
        assert_eq!(found[0].build, None);
    }
    assert!(!ran.exists(), "a program outside the Xcode ran");
}

/// Catches: a warm-up started where `xcrun` is not a file (off macOS: a thread and a
/// directory made for nothing), or where it is a directory.
#[test]
fn warm_runs_nothing_where_xcrun_is_not_a_file() {
    let dir = scratch("edge-warm-none");
    let sandbox = Sandbox {
        isolation: Isolation::None,
        dir: dir.join("lease-warm-up"),
        rules: String::new(),
    };
    let within = Duration::from_secs(5);
    assert!(warm(&dir.join("missing"), vec![None], within, &sandbox).is_none());
    assert!(warm(&dir, vec![None], within, &sandbox).is_none());
    assert!(!sandbox.dir.exists(), "a directory was made for no warm-up");
}

/// Catches: a lookup that fails (exit 1) stopping the warm-up, so the tools after it
/// and the Xcodes after it are never cached, or its directory left behind.
#[test]
fn a_failed_lookup_does_not_stop_the_warm_up() {
    let dir = scratch("edge-warm-fail");
    let log = dir.join("log");
    let logged = format!("append {} ${{DEVELOPER_DIR}} $*", log.display());
    let xcrun = act_fake(
        &dir.join("xcrun"),
        &format!(
            "--find cc\t-\t{logged} ; echo --stderr not found ; exit 1\n\
             *\t-\t{logged}\n"
        ),
    );
    let sandbox = Sandbox {
        isolation: Isolation::None,
        dir: dir.join("lease-warm-up"),
        rules: String::new(),
    };
    warm(
        &xcrun,
        vec![None, Some(PathBuf::from("/x1"))],
        Duration::from_secs(20),
        &sandbox,
    )
    .expect("a thread")
    .join()
    .expect("warmed");
    let want: Vec<String> = ["", "/x1"]
        .iter()
        .flat_map(|dir| WARM_TOOLS.map(|tool| format!("{dir} --find {tool}")))
        .collect();
    let got: Vec<String> = std::fs::read_to_string(&log)
        .expect("log")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(got, want);
    assert!(!sandbox.dir.exists(), "the warm-up's directory stays");
}
