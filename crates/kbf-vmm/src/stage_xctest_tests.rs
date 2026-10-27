//! Tests of `--sim-tests`: the plan of a `Build/Products` tree and its refusals, the copy
//! with per-file hashes into a fake Data volume, and the setup flow. Nothing here needs root
//! or macOS.

use super::*;
use crate::stage_xctest::{TestsError, plan_tests, stage_tests};

const RUN: &str = "IOSFixture_iphonesimulator27.0-arm64.xctestrun";

/// What `xcodebuild build-for-testing` leaves in `Build/Products`: the `.xctestrun`, and the
/// app and the UI-test runner under `Debug-iphonesimulator`.
fn products(dir: &Path) -> PathBuf {
    let p = dir.join("Products");
    write(&p.join(RUN), "<plist/>", 0o644);
    let debug = p.join("Debug-iphonesimulator");
    write(&debug.join("IOSFixture.app/Info.plist"), "plist", 0o644);
    write(&debug.join("IOSFixture.app/IOSFixture"), "app!", 0o755);
    let runner = debug.join("IOSFixtureUITests-Runner.app");
    write(&runner.join("IOSFixtureUITests-Runner"), "runner", 0o755);
    write(
        &runner.join("PlugIns/IOSFixtureUITests.xctest/IOSFixtureUITests"),
        "bundle",
        0o755,
    );
    link("../IOSFixture.app", &debug.join("Alias.app"));
    p
}

fn refusal(r: Result<TestsPlan, StageError>) -> StageError {
    r.expect_err("refused")
}

use crate::stage_xctest::TestsPlan;

/// Catches a plan that misses the `.xctestrun`, takes another name for it, loses a size, or
/// follows a link of the products instead of recording it.
#[test]
fn the_products_directory_is_planned_with_its_one_xctestrun() {
    let dir = tmp("xt-plan");
    let src = products(&dir);
    let p = plan_tests(&src, false).expect("plan");
    assert_eq!(p.xctestrun, RUN);
    assert_eq!(
        p.tree.bytes,
        ["<plist/>", "plist", "app!", "runner", "bundle"]
            .iter()
            .map(|s| s.len() as u64)
            .sum::<u64>()
    );
    assert!(p.tree.nodes.iter().any(|n| matches!(n,
        Node::Link { rel, target } if rel.ends_with("Alias.app") && target == Path::new("../IOSFixture.app"))));
    assert!(p.tree.external_links.is_empty());
    // A rerun of the plan gives the same one.
    assert_eq!(plan_tests(&src, false).expect("again"), p);
}

/// Catches a products directory with no `.xctestrun` (a build that was not for testing), one
/// that holds two (the run would pick by luck), one nested too deep to be the run's, and one
/// that is not a regular file or has a name the guest cannot carry.
#[test]
fn exactly_one_xctestrun_is_required_at_the_top() {
    let dir = tmp("xt-count");
    let p = products(&dir);
    std::fs::remove_file(p.join(RUN)).expect("rm");
    assert_eq!(
        refusal(plan_tests(&p, false)),
        StageError::Tests(TestsError::NoXctestrun(p.clone()))
    );
    write(&p.join("Debug-iphonesimulator/deep.xctestrun"), "x", 0o644);
    assert!(matches!(
        refusal(plan_tests(&p, false)),
        StageError::Tests(TestsError::NoXctestrun(_))
    ));

    write(&p.join("a.xctestrun"), "x", 0o644);
    assert!(plan_tests(&p, false).is_ok());
    write(&p.join("b.xctestrun"), "x", 0o644);
    let e = refusal(plan_tests(&p, false));
    assert_eq!(
        e,
        StageError::Tests(TestsError::TwoXctestruns {
            dir: p.clone(),
            a: "a.xctestrun".into(),
            b: "b.xctestrun".into()
        })
    );
    assert!(e.to_string().contains("exactly one"));
    std::fs::remove_file(p.join("b.xctestrun")).expect("rm");

    // A directory or a link in its place is not a test run.
    std::fs::remove_file(p.join("a.xctestrun")).expect("rm");
    std::fs::create_dir(p.join("c.xctestrun")).expect("dir");
    assert_eq!(
        refusal(plan_tests(&p, false)),
        StageError::Tests(TestsError::NotRegular(p.join("c.xctestrun")))
    );
    std::fs::remove_dir(p.join("c.xctestrun")).expect("rm");
    link(
        "Debug-iphonesimulator/IOSFixture.app/Info.plist",
        &p.join("d.xctestrun"),
    );
    assert_eq!(
        refusal(plan_tests(&p, false)),
        StageError::Tests(TestsError::NotRegular(p.join("d.xctestrun")))
    );
}

/// Catches an `.xctestrun` name that the guest's name rules would not accept being staged
/// (the guest would then run nothing and say nothing).
#[test]
fn the_xctestrun_name_follows_the_name_rules() {
    let dir = tmp("xt-names");
    let longest = format!("{}.xctestrun", "n".repeat(MAX_RUNTIME_NAME_BYTES - 10));
    for (name, ok) in [
        (longest.as_str(), true),
        (
            &format!("{}.xctestrun", "n".repeat(MAX_RUNTIME_NAME_BYTES - 9)),
            false,
        ),
        ("a b.xctestrun", false),
        ("a..b.xctestrun", false),
        (".hidden.xctestrun", false),
        (".xctestrun", false),
        ("\u{fc}.xctestrun", false),
    ] {
        let p = dir.join(format!("p{}", name.len()));
        std::fs::create_dir_all(&p).expect("dir");
        write(&p.join(name), "x", 0o644);
        let r = plan_tests(&p, false);
        if ok {
            assert!(r.is_ok(), "{name}: {r:?}");
        } else {
            assert_eq!(
                refusal(r),
                StageError::Tests(TestsError::BadName(p.join(name))),
                "{name}"
            );
        }
    }
}

/// Catches a link out of the products tree being copied (the guest would read the host's
/// path as root), a special file being staged, the directory itself being a link or a file,
/// and a missing path being reported without a cause.
#[test]
fn a_link_out_a_special_file_and_a_bad_directory_are_refused() {
    let dir = tmp("xt-refuse");
    let p = products(&dir);
    link("/etc", &p.join("Debug-iphonesimulator/out"));
    let e = refusal(plan_tests(&p, false)).to_string();
    assert!(
        e.contains("leaves the app") && e.contains("--allow-external-links"),
        "{e}"
    );
    let allowed = plan_tests(&p, true).expect("allowed");
    assert_eq!(allowed.tree.external_links.len(), 1);
    // An absolute link into the tests' own place in the guest is inside.
    std::fs::remove_file(p.join("Debug-iphonesimulator/out")).expect("rm");
    link(
        "/private/var/kbf-sim/tests/Debug-iphonesimulator/IOSFixture.app",
        &p.join("Debug-iphonesimulator/self"),
    );
    assert!(plan_tests(&p, false).is_ok());
    // ... and one into the app's place is not.
    link(
        "/private/var/kbf-sim/apps/IOSFixture.app",
        &p.join("elsewhere"),
    );
    assert!(plan_tests(&p, false).is_err());
    std::fs::remove_file(p.join("elsewhere")).expect("rm");

    let fifo = p.join("Debug-iphonesimulator/pipe");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("path");
    // SAFETY: `c` is a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    let e = refusal(plan_tests(&p, true));
    assert!(
        matches!(
            &e,
            StageError::Special {
                kind: "named pipe",
                ..
            }
        ),
        "{e}"
    );
    std::fs::remove_file(&fifo).expect("rm");

    link("Products", &dir.join("Alias"));
    assert_eq!(
        refusal(plan_tests(&dir.join("Alias"), false)),
        StageError::Symlink(dir.join("Alias"))
    );
    write(&dir.join("file"), "x", 0o644);
    assert_eq!(
        refusal(plan_tests(&dir.join("file"), false)),
        StageError::Tests(TestsError::NotDirectory(dir.join("file")))
    );
    assert!(matches!(
        refusal(plan_tests(&dir.join("missing"), false)),
        StageError::Io(_)
    ));
}

fn tests_plan(dir: &Path) -> Plan {
    plan_all(None, &[], None, Some(&products(dir)), false).expect("plan")
}

const TESTS: &str = "private/var/kbf-sim/tests";

/// Catches tests staged outside their directory, modes that lose the executable bit of the
/// runner (it would not launch) or keep group write, a tree not owned by root, a manifest
/// without a hash per file, a hash of other bytes than were copied, and `staged.json` not
/// recording the tests.
#[test]
fn the_products_are_copied_with_modes_a_hash_per_file_and_a_record() {
    use sha2::{Digest, Sha256};
    let dir = tmp("xt-stage");
    let plan = tests_plan(&dir);
    let src = dir.join("Products");
    std::fs::set_permissions(
        src.join("Debug-iphonesimulator/IOSFixtureUITests-Runner.app/IOSFixtureUITests-Runner"),
        std::fs::Permissions::from_mode(0o6775),
    )
    .expect("chmod");
    let plan = plan_all(None, &[], None, Some(&src), false).unwrap_or(plan);
    let volume = volume_in(&dir);
    let h = Harness::new(100 * GIB);
    let out = h.run(&volume, &plan).expect("execute");
    let dest = volume.join(TESTS);
    let mut want = listing(&src);
    want.retain(|l| !l.is_empty());
    let got = listing(&dest);
    // Directory modes are the planned ones, files keep their execute bits without group or
    // world write, and the link is a link.
    let runner =
        "f Debug-iphonesimulator/IOSFixtureUITests-Runner.app/IOSFixtureUITests-Runner 755";
    assert!(got.iter().any(|l| l.starts_with(runner)), "{got:?}");
    assert!(
        got.iter()
            .any(|l| l == "l Debug-iphonesimulator/Alias.app -> ../IOSFixture.app"),
        "{got:?}"
    );
    assert!(
        got.iter().any(|l| l.starts_with(&format!("f {RUN} 644"))),
        "{got:?}"
    );
    assert_eq!(got.len(), want.len());
    let t = &out["tests"];
    assert_eq!(t["dest"], "/private/var/kbf-sim/tests");
    assert_eq!(t["xctestrun"], RUN);
    let files = t["files"].as_array().expect("files");
    assert_eq!(files.len(), 5);
    let runner_entry = files
        .iter()
        .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with("-Runner")))
        .expect("runner");
    assert_eq!(runner_entry["bytes"], 6);
    assert_eq!(
        runner_entry["sha256"].as_str().expect("hash"),
        hex::encode(Sha256::digest(b"runner"))
    );
    for f in files {
        let on_disk = std::fs::read(dest.join(f["path"].as_str().expect("p"))).expect("read");
        assert_eq!(
            f["sha256"].as_str().expect("h"),
            hex::encode(Sha256::digest(&on_disk))
        );
    }
    assert_eq!(
        (t["symlinks"].as_u64(), t["bytes"].as_u64()),
        (Some(1), Some(29))
    );
    assert!(
        h.chowned
            .borrow()
            .contains(&volume.join("private/var/kbf-sim/.tests.kbf-staging"))
    );
    assert!(h.chowned.borrow().iter().any(|p| {
        p.ends_with(".tests.kbf-staging/Debug-iphonesimulator/IOSFixture.app/IOSFixture")
    }));
    let m: Value = serde_json::from_str(
        &std::fs::read_to_string(volume.join("private/var/kbf-sim/staged.json")).expect("manifest"),
    )
    .expect("json");
    assert_eq!(m["tests"], *t);
    assert!(
        !volume
            .join("private/var/kbf-sim/.tests.kbf-staging")
            .exists()
    );
}

/// Catches the tests directory keeping an earlier build (the guest would run a test run of
/// another app), and a setup that names no tests leaving the earlier ones for a new Xcode.
#[test]
fn the_directory_holds_exactly_this_setups_tests_and_none_when_none_are_named() {
    let dir = tmp("xt-replace");
    let volume = volume_in(&dir);
    let old = volume.join(TESTS);
    write(&old.join("Old.xctestrun"), "old", 0o644);
    write(&old.join("Debug-iphonesimulator/stale"), "stale", 0o644);
    let h = Harness::new(100 * GIB);
    h.run(&volume, &tests_plan(&dir)).expect("execute");
    let top: Vec<_> = std::fs::read_dir(&old)
        .expect("dir")
        .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        top.contains(&RUN.to_owned()) && !top.contains(&"Old.xctestrun".to_owned()),
        "{top:?}"
    );
    assert!(!old.join("Debug-iphonesimulator/stale").exists());

    // An app-only setup removes them, and the record says there are none.
    let app = fake_app(&dir, "Demo.app");
    let plan = plan_all(None, &[], Some(&app), None, false).expect("plan");
    let out = h.run(&volume, &plan).expect("execute");
    assert!(!old.exists());
    assert_eq!(out["tests"], Value::Null);
    let m: Value = serde_json::from_str(
        &std::fs::read_to_string(volume.join("private/var/kbf-sim/staged.json")).expect("manifest"),
    )
    .expect("json");
    assert_eq!(m["tests"], Value::Null);
    assert!(volume.join("private/var/kbf-sim/apps/Demo.app").is_dir());
}

/// Catches a space check that forgets the tests (a large Products tree would fill the
/// guest's disk half way), and the tests being left out of the plan's total.
#[test]
fn the_tests_count_toward_the_space_needed() {
    let dir = tmp("xt-space");
    let plan = tests_plan(&dir);
    assert_eq!(plan.bytes(), plan.tests.as_ref().expect("tests").tree.bytes);
    assert!(plan.bytes() > 0 && !plan.is_empty());
    let volume = volume_in(&dir);
    let e = Harness::new(plan.bytes() + HEADROOM_BYTES - 1)
        .run(&volume, &plan)
        .expect_err("tight");
    assert!(
        e.contains("not enough room") && e.contains("app and tests"),
        "{e}"
    );
    assert!(!volume.join(TESTS).exists());
}

/// Catches a file swapped for a link after the plan being copied as root, a failed copy
/// leaving a partial tree where the guest looks, and a failed copy destroying the earlier
/// good tests.
#[test]
fn a_source_that_changed_after_the_plan_is_refused_and_leaves_nothing_partial() {
    let dir = tmp("xt-tamper");
    let plan = tests_plan(&dir);
    let volume = volume_in(&dir);
    let dest = volume.join(TESTS);
    write(&dest.join("keep.xctestrun"), "earlier", 0o644);
    let swapped = dir.join("Products/Debug-iphonesimulator/IOSFixture.app/IOSFixture");
    std::fs::remove_file(&swapped).expect("rm");
    link("/etc/hostname", &swapped);
    let e = Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect_err("refused");
    assert!(e.contains("IOSFixture"), "{e}");
    assert!(
        !volume
            .join("private/var/kbf-sim/.tests.kbf-staging")
            .exists()
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("keep.xctestrun")).expect("kept"),
        "earlier"
    );
    assert!(!dest.join(RUN).exists());
}

/// Catches the staging writing through a link a guest planted: a link as the tests
/// directory is removed and replaced, not followed, and a link in the way of the parent is
/// refused.
#[test]
fn a_planted_link_is_neither_followed_nor_written_through() {
    let dir = tmp("xt-planted");
    let plan = tests_plan(&dir);
    let volume = volume_in(&dir);
    let outside = dir.join("outside");
    write(&outside.join("precious"), "host file", 0o644);
    std::fs::create_dir_all(volume.join("private/var/kbf-sim")).expect("dirs");
    link(outside.to_str().expect("utf8"), &volume.join(TESTS));
    Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect("execute");
    assert!(volume.join(TESTS).is_dir() && !volume.join(TESTS).is_symlink());
    assert_eq!(
        std::fs::read_to_string(outside.join("precious")).expect("kept"),
        "host file"
    );
    assert!(!outside.join(RUN).exists());

    let volume = dir.join("volume2");
    std::fs::create_dir_all(volume.join("private/var")).expect("dirs");
    link(
        outside.to_str().expect("utf8"),
        &volume.join("private/var/kbf-sim"),
    );
    let e = Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect_err("refused");
    assert!(e.contains("not a plain directory"), "{e}");
    assert!(!outside.join(RUN).exists());
}

/// Catches an ownership failure on the tests leaving a tree the guest could see, owned by
/// the wrong user.
#[test]
fn a_failed_ownership_change_leaves_no_tests_tree() {
    let dir = tmp("xt-chown");
    let plan = tests_plan(&dir);
    for at in [0, 3] {
        let volume = volume_in(&dir);
        let mut h = Harness::new(100 * GIB);
        // The first changes are Xcode-free: the directory chain, then the tests tree.
        h.chown_fails_at = Some(at);
        let r = h.run(&volume, &plan);
        assert!(r.is_err(), "{at}");
        assert!(!volume.join(TESTS).exists(), "{at}");
        assert!(
            !volume
                .join("private/var/kbf-sim/.tests.kbf-staging")
                .exists(),
            "{at}"
        );
    }
}

/// Catches `stage_tests` with nothing to stage creating the directory it is to remove.
#[test]
fn staging_no_tests_on_a_clean_volume_creates_no_tests_directory() {
    let dir = tmp("xt-none");
    let volume = volume_in(&dir);
    let h = Harness::new(GIB);
    let env = Env {
        free_bytes: &|_| Ok(GIB),
        clone_tree: &|_, _| Err(std::io::Error::other("no")),
        chown_root: &|_| Ok(()),
        remove: &remove_path,
    };
    let _ = &h;
    assert_eq!(stage_tests(&volume, None, &env).expect("none"), Value::Null);
    assert!(!volume.join(TESTS).exists());
}

mod flow {
    use super::*;
    use crate::fake::{FakeSystem, work_with_base};
    use crate::image::{SetupArgs, setup};
    use crate::report::Status;

    /// Catches `--sim-tests` not reaching the volume writer or the simulator daemon, a
    /// refused tree reaching the disk, and the flag not parsing.
    #[test]
    fn the_tests_are_staged_with_the_daemon_and_a_refused_tree_writes_nothing() {
        let work = work_with_base("xt-setup");
        let base = work.join("base");
        let p = products(&work);
        let sys = FakeSystem::new();
        sys.uid.set(0);
        let args = SetupArgs {
            sim_tests: Some(p.clone()),
            ..SetupArgs::default()
        };
        let r = setup(&sys, &base, &args);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!(
            sys.offline_calls.borrow()[0].1,
            ["Library/LaunchDaemons/com.kbf.sim.plist"]
        );
        let calls = sys.stage_calls.borrow();
        assert_eq!(calls[0].1.tests.as_ref().expect("tests").xctestrun, RUN);
        assert!(calls[0].1.app.is_none() && calls[0].1.xcode.is_none());
        drop(calls);

        write(&p.join("second.xctestrun"), "x", 0o644);
        let sys = FakeSystem::new();
        sys.uid.set(0);
        let r = setup(&sys, &base, &args);
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("two *.xctestrun"));
        assert!(sys.offline_calls.borrow().is_empty() && sys.stage_calls.borrow().is_empty());
    }

    /// Catches the flag being unknown to the command line or its value lost on the way to the
    /// setup.
    #[test]
    fn the_flag_reaches_the_setup() {
        use crate::cli::{Cli, Cmd, ImageCmd};
        use clap::Parser;
        let cli = Cli::parse_from([
            "kbf-vmm",
            "image",
            "setup",
            "--base",
            "b",
            "--sim-tests",
            "P",
        ]);
        assert!(matches!(
            &cli.command,
            Cmd::Image(ImageCmd::Setup { sim_tests: Some(p), .. }) if p == Path::new("P")
        ));
        let sys = FakeSystem::new();
        sys.uid.set(0);
        let work = work_with_base("xt-flag");
        let mut out = Vec::new();
        let cli = Cli::parse_from([
            "kbf-vmm",
            "image",
            "setup",
            "--base",
            work.join("base").to_str().expect("utf8"),
            "--sim-tests",
            work.join("no-such-dir").to_str().expect("utf8"),
        ]);
        let _ = crate::app::run(
            cli,
            Some(&sys),
            crate::probe::Timing::instant(),
            &mut out,
            &mut Vec::new(),
        );
        assert!(String::from_utf8_lossy(&out).contains("no-such-dir"));
        assert!(sys.stage_calls.borrow().is_empty());
    }
}
