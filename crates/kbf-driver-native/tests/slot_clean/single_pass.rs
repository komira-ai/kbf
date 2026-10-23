//! The clean in one pass, for real, on a Mac as root: no retry, no recreate, no on-disk
//! quarantine (`src/slots/clean.rs`). One test per behaviour of the single pass.
//!
//! - the home root locked by its user (`chflags uchg`) is cleaned on the first pass;
//! - a file in the `/var/folders` entry outside `0 C T X` is removed;
//! - a file the uid owns in `/Library/Caches` is removed (a raw plant: the sandbox stops
//!   a pool user writing there, so root plants it);
//! - a crash report under another user's `DiagnosticReports` that names the uid is
//!   removed, one that names another uid is not;
//! - a clean that fails leaves the user dirty: not offered, the node reports one slot
//!   fewer, and the slot is cleaned again, and offered, by the recovery thread, with no
//!   lease waiting.
//!
//! The first and the fourth also have a test of necessity: with the step that does the
//! work left out of the clean, the slot stays dirty or the report stays.

use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kbf_driver_native::NativeRuntime;
use kbf_driver_native::slots::SlotUser;
use kbf_driver_native::slots::clean::{Check, Skipped, Step};

use super::{as_user, assert_root, entries, lease_planting, slots_reported};
use crate::support::pool::{Scratch, Users, config, home_of};
use crate::support::{MemoryCas, PROMPT, Spec, stdout};

/// A runtime over one pool user named after the test, optionally with a step left out of its clean.
struct Rig {
    // Declared first, so dropped first: the runtime stops before the user goes.
    rt: NativeRuntime<MemoryCas>,
    cas: Arc<MemoryCas>,
    user: SlotUser,
    scratch: Scratch,
    _users: Users,
}

impl Rig {
    fn new(name: &str, skipped: Option<Skipped>) -> Self {
        assert_root();
        let scratch = Scratch::new(name);
        let users = Users::new(name, 1);
        let user = users.0[0].clone();
        let cas = Arc::new(MemoryCas::default());
        let config = config(&scratch, users.pool());
        let rt = match skipped {
            None => NativeRuntime::new(config, Arc::clone(&cas)),
            Some(skipped) => NativeRuntime::with_skipped_step(config, Arc::clone(&cas), skipped),
        };
        Self {
            rt: rt.expect("runtime"),
            cas,
            user,
            scratch,
            _users: users,
        }
    }

    /// A lease that prints its uid, run to its end (and the slot's clean after it).
    async fn lease(&self, seq: u64) -> kbf_proto::reapi::ActionResult {
        let act = self.scratch.act();
        crate::support::run(&self.rt, &self.cas, seq, &Spec::argv(&[&act, "id"]))
            .await
            .expect("the lease ran")
    }

    /// [`Rig::lease`] whose action waits while `plant` makes a state in the user's slot
    /// ([`lease_planting`]); the lease's result and `plant`'s.
    async fn lease_planting<T>(
        &self,
        seq: u64,
        plant: impl FnOnce() -> T,
    ) -> (kbf_proto::reapi::ActionResult, T) {
        lease_planting(&self.rt, &self.cas, &self.scratch, seq, plant).await
    }

    fn reported(&self) -> Option<String> {
        slots_reported(&self.rt)
    }

    /// Gives `path` to the pool user.
    fn give(&self, path: &Path) {
        std::os::unix::fs::chown(path, Some(self.user.uid), Some(self.user.gid)).expect("chown");
    }

    /// The `/var/folders` entry of the user: the parent of its `DARWIN_USER_DIR`.
    fn entry(&self) -> PathBuf {
        let out = as_user(&self.user, "/usr/bin/getconf", &["DARWIN_USER_DIR"]);
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let parent = Path::new(&dir).parent().expect("an entry").to_owned();
        std::fs::canonicalize(parent).expect("the entry exists")
    }
}

/// Removes `path`, a file or a tree, locks cleared first; nothing if it is not there.
fn remove(path: &Path) {
    let _ = Command::new("/usr/bin/chflags")
        .args(["-R", "nouchg,nouappnd"])
        .arg(path)
        .status();
    let _ = kbf_outputs::remove_tree(path);
}

/// Removes what a test planted outside the user's own folders, also when it panics.
struct Planted(Vec<PathBuf>);

impl Drop for Planted {
    fn drop(&mut self) {
        for path in &self.0 {
            remove(path);
        }
    }
}

/// The home root locked by its user, then a lease; what the clean left.
struct Locked {
    reported: Option<String>,
    flags: u32,
    left: Vec<String>,
    second: Option<String>,
}

/// Plants a file and a folder in the user's home, and as the user locks the home root
/// (`chflags uchg`: no entry of it can be added or removed), runs a lease and reads the
/// home and the node's slot count after its clean. With `next`, a second lease follows.
async fn locked_home(name: &str, skipped: Option<Skipped>, next: bool) -> Locked {
    let rig = Rig::new(name, skipped);
    let home = home_of(&rig.user);
    let (first, ()) = rig
        .lease_planting(1, || {
            std::fs::write(home.join("kbf-locked"), b"x").expect("file");
            std::fs::create_dir(home.join("kbf-locked-dir")).expect("dir");
            std::fs::write(home.join("kbf-locked-dir/inner"), b"x").expect("inner");
            for path in ["kbf-locked", "kbf-locked-dir", "kbf-locked-dir/inner"] {
                rig.give(&home.join(path));
            }
            let locked = as_user(
                &rig.user,
                "/usr/bin/chflags",
                &["uchg", &home.to_string_lossy()],
            );
            assert!(
                locked.status.success(),
                "chflags uchg on the home: {locked:?}"
            );
        })
        .await;
    assert_eq!(first.exit_code, 0);
    let reported = rig.reported();
    let flags = std::fs::symlink_metadata(&home).expect("home").st_flags();
    let left = entries(&home);
    let second = if next {
        let again = rig.lease(2).await;
        Some(stdout(&rig.cas, &again))
    } else {
        None
    };
    remove(&home.join("kbf-locked-dir"));
    Locked {
        reported,
        flags,
        left,
        second,
    }
}

/// The home root locked by its user. Catches a clean that sweeps the home before it
/// clears the flags on the home root (an `uchg` directory cannot lose an entry): with the
/// home locked and holding a file and a folder, the lease's clean succeeds on its single
/// pass, the home is empty with no flags, and the next lease runs on the same slot.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_home_locked_by_its_user_is_cleaned_on_the_single_pass() {
    let seen = locked_home("locked-home", None, true).await;
    assert_eq!(seen.reported.as_deref(), Some("1"), "the slot is dirty");
    assert_eq!(seen.flags, 0, "the home root is still locked");
    assert_eq!(seen.left, Vec::<String>::new());
    assert!(seen.second.is_some_and(|out| !out.trim().is_empty()));
}

/// The flags step is needed for the locked home: without it the removal cannot empty the
/// locked home, the one pass fails, and the slot is dirty (the node reports no slot).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_flags_step_the_locked_home_leaves_the_slot_dirty() {
    let skipped = Skipped {
        skip: Step::Flags,
        check: Check::Honest,
    };
    let seen = locked_home("locked-home-without-flags", Some(skipped), false).await;
    assert_eq!(
        seen.reported.as_deref(),
        Some("0"),
        "the slot was clean although the home root was locked and the flags step left out \
         (the home holds {:?})",
        seen.left
    );
}

/// A file and a folder in the `/var/folders` entry root, outside `0 C T X`. Catches
/// a sweep of the entry that empties only the folders it knows (what the user left in
/// the root survived every clean): both are gone after the lease, the `0` folder is still
/// there, and the next lease can ask for its temporary folder.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_file_in_the_entry_root_outside_its_folders_is_removed() {
    let rig = Rig::new("entry-root", None);
    let entry = rig.entry();
    let file = entry.join("kbf-entry-stray");
    let stray = entry.join("kbf-entry-stray-dir");
    let _planted = Planted(vec![file.clone(), stray.clone()]);
    let (first, ()) = rig
        .lease_planting(1, || {
            std::fs::write(&file, b"x").expect("file");
            std::fs::create_dir(&stray).expect("dir");
            std::fs::write(stray.join("inner"), b"x").expect("inner");
            for path in [&file, &stray, &stray.join("inner")] {
                rig.give(path);
            }
        })
        .await;
    assert_eq!(first.exit_code, 0);
    let left = entries(&entry);
    let reported = rig.reported();
    let second = crate::support::run(
        &rig.rt,
        &rig.cas,
        2,
        &Spec::argv(&["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"]),
    )
    .await
    .expect("the second lease ran");
    assert!(
        !left.iter().any(|n| n.starts_with("kbf-entry-stray")),
        "the entry root still holds {left:?}"
    );
    assert!(
        left.iter().any(|n| n == "0"),
        "the 0 folder was removed: {left:?}"
    );
    assert_eq!(reported.as_deref(), Some("1"));
    assert_eq!(
        second.exit_code,
        0,
        "{}",
        crate::support::stderr(&rig.cas, &second)
    );
}

/// A file the uid owns in `/Library/Caches`, planted raw by root (the sandbox
/// refuses a pool user the write). Catches a shared sweep without `/Library/Caches`: the
/// uid's file is gone after the lease and a root-owned file beside it, the control, is
/// not (a sweep that removes everything in the place would pass the first half alone).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_file_the_uid_owns_in_library_caches_is_removed() {
    let rig = Rig::new("library-caches", None);
    let id = std::process::id();
    let mine = PathBuf::from(format!("/Library/Caches/kbf-caches-{id}"));
    let control = PathBuf::from(format!("/Library/Caches/kbf-caches-control-{id}"));
    let _planted = Planted(vec![mine.clone(), control.clone()]);
    std::fs::write(&mine, b"x").expect("plant");
    rig.give(&mine);
    std::fs::write(&control, b"x").expect("control");
    let first = rig.lease(1).await;
    assert_eq!(first.exit_code, 0);
    assert!(!mine.exists(), "the uid's file is still in /Library/Caches");
    assert!(
        control.exists(),
        "a root-owned file was removed from /Library/Caches"
    );
    assert_eq!(rig.reported().as_deref(), Some("1"));
}

/// A crash report under a user's `DiagnosticReports` in `/Users`, as the console user's
/// crash reporter writes one about a process of another uid.
struct Reports {
    /// The directory made in `/Users`.
    user_dir: PathBuf,
    /// A root-owned report whose header names the uid.
    names_uid: PathBuf,
    /// A root-owned report whose header names another uid.
    names_other: PathBuf,
    /// A report that does not name the uid, owned by it.
    owned: PathBuf,
}

impl Reports {
    fn plant(rig: &Rig, id: u32) -> (Self, Planted) {
        let user_dir = Path::new("/Users").join(format!("kbf-crash-{id}"));
        let reports = user_dir.join("Library/Logs/DiagnosticReports");
        std::fs::create_dir_all(&reports).expect("reports");
        let report = |uid: u32| {
            format!(
                "{{\"app_name\":\"canary\",\"bug_type\":\"309\"}}\n{{\"pid\":912,\"userID\":{uid},\"procName\":\"canary\"}}\n"
            )
        };
        let reports = Self {
            names_uid: reports.join(format!("canary-{id}.ips")),
            names_other: reports.join(format!("canary-other-{id}.ips")),
            owned: reports.join(format!("canary-owned-{id}.ips")),
            user_dir,
        };
        std::fs::write(&reports.names_uid, report(rig.user.uid)).expect("report");
        std::fs::write(&reports.names_other, report(rig.user.uid + 1)).expect("report");
        std::fs::write(&reports.owned, report(rig.user.uid + 1)).expect("report");
        rig.give(&reports.owned);
        let planted = Planted(vec![reports.user_dir.clone()]);
        (reports, planted)
    }
}

/// One lease on a user with [`Reports`] planted: which of them are left after its clean.
async fn crash_reports(name: &str, skipped: Option<Skipped>) -> (bool, bool, bool) {
    let rig = Rig::new(name, skipped);
    let (reports, _planted) = Reports::plant(&rig, std::process::id());
    let first = rig.lease(1).await;
    assert_eq!(first.exit_code, 0);
    assert!(reports.user_dir.is_dir());
    (
        reports.names_uid.exists(),
        reports.names_other.exists(),
        reports.owned.exists(),
    )
}

/// Crash reports under another user's `DiagnosticReports`. Catches a clean that
/// looks for reports only in the pool user's own home (the report lands under the console
/// user's), one that removes by file name or removes everything, and one that ignores the
/// owner: the report that names the uid and the one the uid owns are gone, the report that
/// names another uid, owned by root, stays.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_crash_report_naming_the_uid_under_another_users_reports_is_removed() {
    let (names_uid, names_other, owned) = crash_reports("crash-reports", None).await;
    assert!(!names_uid, "the report that names the uid is still there");
    assert!(!owned, "the report the uid owns is still there");
    assert!(names_other, "a report that names another uid was removed");
}

/// The crash step is needed for those reports: without it the report that names the uid
/// is still there after the clean.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn without_the_crash_step_the_report_naming_the_uid_stays() {
    let skipped = Skipped {
        skip: Step::Crashes,
        check: Check::Blind,
    };
    let (names_uid, names_other, owned) =
        crash_reports("crash-reports-without-step", Some(skipped)).await;
    assert!(
        names_uid,
        "the report that names the uid went although the crash step was left out"
    );
    assert!(names_other && owned);
}

/// A failed step. Catches a clean that gives a slot back although a step failed (the
/// next lease would find what it could not remove), a failed clean that takes the slot
/// out for good (the quarantine this replaced), one that retries inside the pass, and one
/// that is not cleaned again when offered: with a root-owned locked file in the home, the
/// pass fails, the node reports no slot and the next lease is not given the slot; once
/// the file can be removed, the next offer cleans the slot again, the node reports it,
/// and a lease runs on it as that uid. Nothing on disk records the dirty state.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_failed_clean_leaves_the_user_dirty_and_the_recovery_cleans_it_again() {
    let rig = Rig::new("dirty", None);
    let home = home_of(&rig.user);
    let stuck = home.join("kbf-ci-stuck");
    let (first, ()) = rig
        .lease_planting(1, || {
            std::fs::write(&stuck, b"stuck").expect("stuck file");
            crate::support::pool::run("/usr/bin/chflags", &["uchg", &stuck.to_string_lossy()]);
        })
        .await;
    assert_eq!(first.exit_code, 0);
    let reported = rig.reported();
    let act = rig.scratch.act();
    let second = tokio::time::timeout(
        Duration::from_secs(3),
        crate::support::run(&rig.rt, &rig.cas, 2, &Spec::argv(&[&act, "id"])),
    )
    .await;
    let after_second = entries(&home);
    let record = std::fs::read_dir(crate::support::pool::HOMES)
        .expect("homes")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".kbf-"))
        .count();
    crate::support::pool::run("/usr/bin/chflags", &["nouchg", &stuck.to_string_lossy()]);
    let third = tokio::time::timeout(
        PROMPT * 3,
        crate::support::run(&rig.rt, &rig.cas, 3, &Spec::argv(&[&act, "id"])),
    )
    .await
    .expect("the next offer got the slot once the file could go")
    .expect("the next lease ran");
    let reported_after = rig.reported();
    assert_eq!(
        reported.as_deref(),
        Some("0"),
        "a dirty slot is still reported"
    );
    assert!(second.is_err(), "a lease ran on the dirty slot: {second:?}");
    assert_eq!(
        after_second,
        ["kbf-ci-stuck"],
        "the stuck file is not the only thing left"
    );
    assert_eq!(
        record, 0,
        "something about the dirty slot was written beside the homes"
    );
    assert_eq!(third.exit_code, 0);
    assert_eq!(stdout(&rig.cas, &third), format!("{}\n", rig.user.uid));
    assert_eq!(reported_after.as_deref(), Some("1"));
    assert_eq!(entries(&home), Vec::<String>::new());
    assert_eq!(
        std::fs::symlink_metadata(&home).expect("home").uid(),
        rig.user.uid
    );
}

/// The `action_slots` value of the newest report of the watch.
fn reported_slots(
    reports: &tokio::sync::watch::Receiver<kbf_daemon::DriverReport>,
) -> Option<String> {
    let report = reports.borrow();
    (report.entries.iter())
        .find(|(key, _)| key == kbf_driver_native::slots::CAPABILITY)
        .map(|(_, value)| value.clone())
}

/// Waits until the newest report says `want` slots; the values it said on the way.
async fn report_says(
    reports: &mut tokio::sync::watch::Receiver<kbf_daemon::DriverReport>,
    want: &str,
    within: Duration,
) -> Result<Vec<String>, Vec<String>> {
    let mut seen: Vec<String> = reported_slots(reports).into_iter().collect();
    let give_up = tokio::time::Instant::now() + within;
    while seen.last().map(String::as_str) != Some(want) {
        if tokio::time::timeout_at(give_up, reports.changed())
            .await
            .is_err()
        {
            return Err(seen);
        }
        seen.extend(reported_slots(reports));
    }
    Ok(seen)
}

/// Recovery with no lease. Catches a node whose every slot is dirty that never comes back
/// (it takes no lease, so no lease cleans it, and no Xcode survey changes to carry a new
/// report): the slot is dirty after a lease, the report pushed through the slots' part
/// of the driver watch says 0, and once the file that stopped the clean can go, the
/// recovery thread cleans the slot again with no lease waiting and the report says 1.
/// No lease runs on the slot in between: the home is empty because the recovery
/// emptied it.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_node_whose_every_slot_is_dirty_recovers_by_itself_and_reports_it() {
    let rig = Rig::new("recover", None);
    let (driver, mut reports) = kbf_daemon::DriverWatch::new();
    rig.rt.report_slots_through(
        driver
            .part(kbf_driver_native::slots::SOURCE)
            .expect("the part"),
    );
    assert_eq!(reported_slots(&reports).as_deref(), Some("1"));
    let home = home_of(&rig.user);
    let stuck = home.join("kbf-ci-stuck");
    let (first, ()) = rig
        .lease_planting(1, || {
            std::fs::write(&stuck, b"stuck").expect("stuck file");
            crate::support::pool::run("/usr/bin/chflags", &["uchg", &stuck.to_string_lossy()]);
        })
        .await;
    assert_eq!(first.exit_code, 0);
    let down = report_says(&mut reports, "0", PROMPT).await;
    assert!(
        down.is_ok(),
        "the report never said the slot was dirty: {down:?}"
    );
    assert_eq!(entries(&home), ["kbf-ci-stuck"]);
    crate::support::pool::run("/usr/bin/chflags", &["nouchg", &stuck.to_string_lossy()]);
    let up = report_says(&mut reports, "1", PROMPT * 3).await;
    assert!(
        up.is_ok(),
        "the report never said the slot was back: {up:?}"
    );
    assert_eq!(entries(&home), Vec::<String>::new());
    assert_eq!(rig.reported().as_deref(), Some("1"));
    let again = rig.lease(2).await;
    assert_eq!(stdout(&rig.cas, &again), format!("{}\n", rig.user.uid));
    drop(driver);
}
