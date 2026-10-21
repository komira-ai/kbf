//! The scratch a killed stage leaves in `work/`, and what `run` does about it when it starts.
//!
//! A stage makes `work/<commit>.partial/` and `work/<commit>.check/` and removes them
//! whatever its outcome, but a kill (a crash, the OOM killer, a reboot) gives it no chance
//! to. `Partial::create` takes only a directory it makes itself, so the stage of that commit
//! was refused for ever. `Helper::recover`, which `run` calls once before its first pass,
//! is where the leftovers are removed. The pipeline in these tests is the real one
//! ([`PipelineFetcher`]).

use std::path::Path;

use super::pipeline_fetcher_tests::PipelineFetcher;
use super::*;

/// A frontend host running A, whose fetches are the real pipeline's.
fn real_frontend(name: &str) -> Rig {
    let mut rig = Rig::frontend(name);
    let fetcher = PipelineFetcher::new(&rig.layout);
    rig.helper.set_fetcher(Box::new(fetcher));
    rig
}

/// The host's `work/`.
fn work_dir(rig: &Rig) -> PathBuf {
    rig.layout.join(&rig.layout.work_dir())
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("dir")
        .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Whether release `commit` holds `file`.
fn holds(rig: &Rig, commit: &str, file: &str) -> bool {
    rig.root
        .join("var/lib/kbf-installer/releases")
        .join(commit)
        .join(file)
        .is_file()
}

/// Catches a commit that is refused for ever after a kill. A stage killed in its fetch left
/// `work/<commit>.partial/` (one killed in its checks left `.check/` too); `run` starts
/// again, and the frontend's update of that commit must stage. Before the sweep every
/// attempt was refused with "File exists" until a person removed the directory by hand.
///
/// Red on main: `recover` removes nothing, `Partial::create` meets `<B>.partial` and refuses
/// (`cannot use .../work/<B>.partial: File exists`), the fetch is `Rejected`, and the update
/// is refused with `stage: ...`, so the first assertion fails.
///
/// Mutant it is meant to catch (reasoned, not run: the change is not compiled locally): the
/// sweep call left out of `recover` (the same red).
#[test]
fn a_commit_whose_stage_was_killed_stages_after_run_starts_again() {
    let mut rig = real_frontend("sweep-then-stage");
    let work = work_dir(&rig);
    let partial = work.join(format!("{B}.partial"));
    let check = work.join(format!("{B}.check"));
    fs::create_dir_all(partial.join("dl")).expect("a killed stage's directory");
    fs::write(partial.join("dl/kbf.tar.gz"), b"half a download").expect("its download");
    fs::create_dir_all(check.join("gh")).expect("a killed stage's checks");

    rig.helper.recover().expect("run's start");
    rig.order(1, Verb::FrontendUpdate, B);
    rig.cycle(T0);

    assert_eq!(rig.refusal(), "", "the update is taken");
    assert!(holds(&rig, B, "VERIFIED"), "it was installed");
    assert_eq!(rig.state().staged, Some(c(B)));
    let left = names(&work);
    assert!(left.is_empty(), "scratch left: {left:?}");
}

/// Catches a sweep that removes more than a killed stage's two kinds of scratch, or follows
/// a link out of `work/`. Of what `work/` holds, `<commit>.partial` and `<commit>.check` go
/// (a directory with all of it, a file, a link itself) and everything else stays: a file, a
/// directory whose name only contains the suffix, a name that ends in the word with no dot,
/// a link without the suffix. A link inside a swept directory and a swept link leave what
/// they point at alone, and the releases and the state are as they were.
///
/// Red on main: nothing is swept, so the names the test lists as gone are all still there.
///
/// Mutants it is meant to catch (reasoned, not run): the suffix test without its dot (red:
/// `xpartial` goes); `.check` left out of the test (red: the two `.check` entries stay); a
/// sweep that follows links (red: `outside/keep.txt` goes); a sweep of the whole of `work/`
/// (red: the kept names go).
#[test]
fn the_sweep_removes_the_two_suffixes_and_nothing_else_and_follows_no_link() {
    let mut rig = Rig::new("sweep-scope");
    let work = work_dir(&rig);
    let outside = rig.root.join("outside");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(outside.join("keep.txt"), b"not scratch").expect("outside file");

    // Scratch: both directories a killed stage leaves (the first holds a link out), and a
    // file and a link that carry the suffix.
    let partial = work.join(format!("{B}.partial"));
    fs::create_dir_all(partial.join("dl")).expect("partial");
    symlink(&outside, partial.join("dl/out")).expect("a link out of the scratch");
    fs::create_dir_all(work.join(format!("{B}.check/gh"))).expect("check");
    fs::write(work.join(format!("{C}.partial")), b"a file").expect("a file");
    symlink(&outside, work.join(format!("{C}.check"))).expect("a link with the suffix");

    // Not scratch.
    let old = work.join(format!("{A}.partial.old"));
    fs::create_dir_all(&old).expect("a name that only contains the suffix");
    fs::write(old.join("keep"), b"kept").expect("its file");
    fs::create_dir_all(work.join("xpartial")).expect("a name ending in the word, no dot");
    fs::write(work.join("notes.txt"), b"notes").expect("a file");
    symlink(&outside, work.join("to-outside")).expect("a link without the suffix");

    rig.helper.recover().expect("run's start");

    let kept = [
        format!("{A}.partial.old"),
        "notes.txt".to_owned(),
        "to-outside".to_owned(),
        "xpartial".to_owned(),
    ];
    assert_eq!(names(&work), kept);
    assert_eq!(fs::read(old.join("keep")).expect("kept"), b"kept");
    let outside_file = fs::read(outside.join("keep.txt")).expect("outside");
    assert_eq!(outside_file, b"not scratch");
    assert_eq!(rig.current(), Some(c(A)), "the release is as it was");
    assert!(holds(&rig, A, "kbf"), "the release is still there");
}

/// Catches a start that dies, or says nothing, when `work/` cannot be listed (a file stands
/// where the directory belongs). The start goes on, since the service must still take
/// orders, and the failure is logged and is not a refusal: nothing was ordered that it
/// could undo, and a refusal would hide the host's real state in `status.json`.
///
/// Red on main: `recover` never looks at `work/`, so nothing is logged.
///
/// Mutants it is meant to catch (reasoned, not run): the error of the sweep dropped (red:
/// nothing in the log); the error returned from `recover` (red: `expect` fails); the error
/// made a refusal (red: `helper.json` says "refused").
#[test]
fn a_work_directory_that_cannot_be_listed_is_logged_not_a_failed_start() {
    let mut rig = Rig::new("sweep-unlistable");
    fs::write(work_dir(&rig), b"not a directory").expect("a file where work/ belongs");
    rig.helper.recover().expect("the start goes on");
    let mut log = Vec::new();
    rig.helper.drain_log(&mut log);
    let log = String::from_utf8(log).expect("text");
    assert!(log.contains("work sweep: "), "{log}");
    assert!(log.contains("/work: "), "{log}");
    let path = rig.root.join("var/lib/kbf-installer/helper.json");
    let memory = fs::read_to_string(path).expect("memory");
    assert!(!memory.contains("refused"), "{memory}");
}

/// Catches a start that dies when the lock cannot be taken (a directory stands where the
/// lock file belongs): the sweep's failure is logged, and the start goes on to the repair,
/// which fails the same way and is the refusal the node reports.
///
/// Not red on main: nothing there takes the lock before the repair. It is the one test that
/// reaches the sweep's own lock error, and the log line tells it from the repair's refusal.
///
/// Mutant it is meant to catch (reasoned, not run): the sweep's error dropped (red: nothing
/// in the log).
#[test]
fn a_lock_that_cannot_be_taken_is_logged_by_the_sweep_and_the_start_goes_on() {
    let mut rig = Rig::new("sweep-no-lock");
    let lock = rig.root.join("run/kbf-installer/lock");
    fs::remove_file(&lock).expect("the lock file");
    fs::create_dir(&lock).expect("a directory where the lock file belongs");
    rig.helper.recover().expect("the start goes on");
    let mut log = Vec::new();
    rig.helper.drain_log(&mut log);
    let log = String::from_utf8(log).expect("text");
    assert!(log.contains("work sweep: "), "{log}");
    assert!(log.contains("cannot lock"), "{log}");
    let path = rig.root.join("var/lib/kbf-installer/helper.json");
    let memory = fs::read_to_string(path).expect("memory");
    assert!(memory.contains("cannot lock"), "no refusal: {memory}");
}

/// Catches a sweep made in a pass. A stage takes no lock, and `setup` stages the first
/// release in a process of its own, so a sweep at every pass could remove the scratch of a
/// stage that is running. Only the start of `run`, before this process stages anything,
/// sweeps: a directory that appears after it is left by the passes.
///
/// Not red on main (nothing sweeps there): it pins the choice of where the sweep is made.
///
/// Mutant it is meant to catch (reasoned, not run): the sweep called from `cycle` (red: the
/// directory is gone after the pass).
#[test]
fn a_pass_never_sweeps_what_a_stage_of_another_process_may_be_using() {
    let mut rig = Rig::new("sweep-start-only");
    rig.helper.recover().expect("run's start");
    let live = work_dir(&rig).join(format!("{B}.partial"));
    fs::create_dir_all(live.join("dl")).expect("a live stage's directory");
    rig.cycle(T0);
    rig.cycle(T0 + 3);
    assert!(live.join("dl").is_dir(), "a pass removed it");
}
