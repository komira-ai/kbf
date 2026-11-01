//! The Linux backend keeps the last N releases: updates over the real backend on a scratch
//! root, with the update core asking for the prune after each confirm.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use kbf_installer::CommitId;
use kbf_installer::fixtures::{file, tar_gz};
use kbf_installer::helper::service::Procs;
use kbf_installer::retain::Retained;

use super::{A, Ab, Answer, Borrowed, LocalRequest, Rig, Trust, binary, what_kbf_says};
use crate::fixed_mark::mark;
use crate::local::answer::Code;
use crate::local::backend::Backend;

/// Seconds after which a release was laid out: every release gets its own time, so the order
/// of age is the order of the test and not of the clock of the machine.
const BASE: u64 = 1_700_000_000;

fn commit_n(n: u32) -> String {
    format!("{n:02}{}", "d".repeat(38))
}

fn version_n(n: u32) -> String {
    format!("0.2.{n}+g{}-1", &commit_n(n)[..12])
}

fn id(commit: &str) -> CommitId {
    CommitId::new(commit).unwrap()
}

/// Lay the release directory `commit` out as the time `at` seconds after BASE.
fn age(rig: &Rig, commit: &str, at: u64) {
    let dir = rig.releases().join(commit);
    fs::File::open(&dir)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(BASE + at))
        .unwrap();
}

/// A release directory with a binary, as the installer lays one out, laid out at `at`.
fn seed(rig: &Rig, commit: &str, at: u64) {
    let dir = rig.releases().join(commit);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("kbf"), binary(commit)).unwrap();
    age(rig, commit, at);
}

/// The release directories, by name.
fn dirs(rig: &Rig) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(rig.releases())
        .unwrap()
        .map(Result::unwrap)
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

type Notes = Rc<RefCell<Vec<String>>>;

fn update_core<'a>(rig: &'a Rig, keep: usize) -> (Ab<Borrowed<'a>>, Notes) {
    let uid = std::os::unix::fs::MetadataExt::uid(&fs::metadata(&*rig.dir).unwrap());
    let notes: Notes = Rc::default();
    let told = Rc::clone(&notes);
    let ab = Ab::new(
        Borrowed(&rig.backend),
        rig.dir.join("state.json"),
        Trust::owned_by(uid, &rig.dir),
        600,
        Box::new(|| 1_000),
    )
    .keeping(
        keep,
        Box::new(move |text| told.borrow_mut().push(text.to_owned())),
    );
    (ab, notes)
}

fn ask(ab: &Ab<Borrowed<'_>>, line: &str) -> Answer {
    ab.handle(&LocalRequest::parse(line).unwrap())
}

/// Release `n` is offered by the channel, fetched, verified and laid out by an install, and
/// put on trial by a switch.
fn stage_and_switch(rig: &Rig, ab: &Ab<Borrowed<'_>>, n: u32) {
    let commit = commit_n(n);
    rig.offer(&commit, &tar_gz(&[file("kbf", &binary(&format!("r{n}")))]));
    *rig.probe.borrow_mut() = what_kbf_says(&commit);
    let version = version_n(n);
    let staged = ask(ab, &format!("install kbf={version} from stable"));
    assert!(
        matches!(&staged, Answer::Staged { current: false, .. }),
        "{staged:?}"
    );
    age(rig, &commit, u64::from(n) * 10);
    let switched = ask(ab, &format!("switch kbf={version}"));
    assert!(matches!(switched, Answer::Switched { .. }), "{switched:?}");
}

fn confirm(ab: &Ab<Borrowed<'_>>, n: u32, last_good: Option<u32>) {
    let mut line = format!("confirm kbf={}", version_n(n));
    if let Some(good) = last_good {
        line.push_str(&format!(" last_good={}", version_n(good)));
    }
    assert_eq!(
        ask(ab, &line),
        Answer::Confirmed {
            version: version_n(n)
        }
    );
}

/// After nine updates over a host that began on release A with a keep of four, what remains is
/// the release that runs, the one before it, the pinned release, the coordinator's last good
/// (named by each confirm from the third on) and the four newest others; the oldest two are
/// gone.
///
/// Catches: a prune that ignores the pin, the last good or the previous release, or keeps one
/// too many or too few.
#[test]
fn after_many_updates_only_the_newest_and_the_ones_that_must_stay_remain() {
    let rig = Rig::new("retain-many");
    age(&rig, A, 0);
    let (ab, notes) = update_core(&rig, 4);
    for n in 1..=9 {
        stage_and_switch(&rig, &ab, n);
        if n == 1 {
            let file = rig.backend.retain_file().unwrap();
            Retained::update(&file, |r| {
                r.pinned.insert(id(&commit_n(1)));
            })
            .unwrap();
        }
        let on_trial = dirs(&rig);
        assert!(
            on_trial.contains(&commit_n(n)),
            "the release on trial is laid out"
        );
        confirm(&ab, n, (n >= 3).then_some(2));
    }
    let mut expected: Vec<String> = [9, 8, 7, 6, 5, 4, 1, 2].map(commit_n).to_vec();
    expected.sort();
    assert_eq!(dirs(&rig), expected, "notes: {:?}", notes.borrow());
    assert_eq!(rig.link("current"), Some(commit_n(9)));
    assert_eq!(rig.link("previous"), Some(commit_n(8)));
    assert!(!dirs(&rig).contains(&A.to_owned()), "the oldest went first");
    assert!(
        notes
            .borrow()
            .iter()
            .any(|t| t.contains("prune: ") && t.contains("keeping: ")),
        "{:?}",
        notes.borrow()
    );
    let file = Retained::read(&rig.backend.retain_file().unwrap()).unwrap();
    assert_eq!(file.last_good, Some(id(&commit_n(2))));
    assert!(file.pinned.contains(&id(&commit_n(1))));
}

/// Nothing is removed while a swap is on trial, whatever the count: the installer's gc says it
/// removes nothing and every release directory is still there; the confirm then prunes.
///
/// Catches: the installer's gc pruning the other releases while a swap is on trial.
#[test]
fn nothing_is_removed_while_a_swap_is_on_trial() {
    let rig = Rig::new("retain-trial");
    for n in 20..28 {
        seed(&rig, &commit_n(n), u64::from(n));
    }
    age(&rig, A, 0);
    let (ab, _) = update_core(&rig, 2);
    stage_and_switch(&rig, &ab, 1);
    let before = dirs(&rig);
    assert_eq!(before.len(), 10);
    let said = rig.backend.prune(2).unwrap();
    assert!(said.contains("removing nothing"), "{said}");
    assert!(said.contains(&commit_n(1)), "{said}");
    assert_eq!(dirs(&rig), before, "the prune on trial");

    confirm(&ab, 1, None);
    let after = dirs(&rig);
    // The release that runs, the one before it and the two newest others.
    let mut expected = vec![commit_n(1), A.to_owned(), commit_n(27), commit_n(26)];
    expected.sort();
    assert_eq!(after, expected);
}

/// A release a process runs from is kept however old it is; the same release not run from is
/// removed with the rest.
///
/// Catches: the running releases not passed to the gc.
#[test]
fn a_release_a_process_runs_from_is_kept() {
    let rig = Rig::new("retain-running");
    for n in 20..28 {
        seed(&rig, &commit_n(n), u64::from(n));
    }
    age(&rig, A, 100);
    rig.running
        .borrow_mut()
        .push(rig.releases().join(commit_n(20)).join("kbf"));
    let said = rig.backend.prune(2).unwrap();
    let mut expected = vec![A.to_owned(), commit_n(20), commit_n(27), commit_n(26)];
    expected.sort();
    assert_eq!(dirs(&rig), expected, "{said}");
    assert!(said.contains("keeping: "), "{said}");
    assert!(!said.contains('\n'), "one line: {said}");

    rig.running.borrow_mut().clear();
    rig.backend.prune(2).unwrap();
    let mut expected = vec![A.to_owned(), commit_n(27), commit_n(26)];
    expected.sort();
    assert_eq!(dirs(&rig), expected);
}

struct Blind;

impl Procs for Blind {
    fn exe_of(&self, _pid: u32) -> io::Result<PathBuf> {
        Err(io::ErrorKind::NotFound.into())
    }

    fn executables(&self) -> io::Result<Vec<PathBuf>> {
        Err(io::Error::other("no process table"))
    }
}

/// A host that cannot tell which releases run removes nothing, and says so.
///
/// Catches: a process table that cannot be read taken for one where nothing runs.
#[test]
fn a_host_that_cannot_tell_what_runs_removes_nothing() {
    let mut rig = Rig::new("retain-blind");
    for n in 20..28 {
        seed(&rig, &commit_n(n), u64::from(n));
    }
    rig.backend.procs = Box::new(Blind);
    let before = dirs(&rig);
    let fault = rig.backend.prune(2).unwrap_err();
    assert_eq!(fault.code, Code::StateUnavailable);
    assert!(
        fault.detail.contains("cannot tell which releases run"),
        "{fault:?}"
    );
    assert!(fault.detail.contains("no process table"), "{fault:?}");
    assert_eq!(dirs(&rig), before);
}

/// A retain file that cannot be parsed may hold a pin: the prune fails and removes nothing, and
/// the listing fails with it (the query then leaves the releases out).
///
/// Catches: a retain file that cannot be parsed taken for an empty one.
#[test]
fn an_unparseable_retain_file_removes_nothing_and_the_query_still_answers() {
    let rig = Rig::new("retain-garbled");
    for n in 20..28 {
        seed(&rig, &commit_n(n), u64::from(n));
    }
    let file = rig.backend.retain_file().unwrap();
    fs::write(&file, "{ not json").unwrap();
    let before = dirs(&rig);
    let fault = rig.backend.prune(2).unwrap_err();
    assert_eq!(fault.code, Code::StateUnavailable);
    assert!(fault.detail.contains("cannot be parsed"), "{fault:?}");
    assert_eq!(dirs(&rig), before);
    assert_eq!(rig.backend.kept().unwrap_err().code, Code::StateUnavailable);

    let (ab, _) = update_core(&rig, 2);
    match ask(&ab, "query") {
        Answer::Queried(view) => {
            assert!(view.releases.is_empty());
            assert_eq!(view.commit.as_deref(), Some(A));
        }
        other => panic!("{other:?}"),
    }
}

/// The file is `retain.json` in the installer's state directory, where the installer reads it.
#[test]
fn the_retain_file_is_in_the_installers_state_directory() {
    let rig = Rig::new("retain-place");
    assert_eq!(
        rig.backend.retain_file(),
        Some(rig.root().join("var/lib/kbf-installer/retain.json"))
    );
}

/// A query lists the releases kept, newest first, each with its commit, version and why it stays;
/// a release laid out by an install carries the version it reported. The release that was
/// never named has none.
#[test]
fn a_query_lists_the_kept_releases_with_commit_and_version() {
    let rig = Rig::new("retain-query");
    age(&rig, A, 0);
    let (ab, _) = update_core(&rig, 4);
    for n in 1..=2 {
        stage_and_switch(&rig, &ab, n);
        confirm(&ab, n, None);
    }
    let Answer::Queried(view) = ask(&ab, "query") else {
        panic!("not a query answer");
    };
    let shown: Vec<(String, bool)> = view
        .releases
        .iter()
        .map(|k| (k.commit.to_string(), k.version.is_some()))
        .collect();
    assert_eq!(
        shown,
        [
            (commit_n(2), true),
            (commit_n(1), true),
            (A.to_owned(), false)
        ]
    );
    assert_eq!(view.releases[0].why, ["current"]);
    assert_eq!(view.releases[1].why, ["previous"]);
    assert!(view.releases[2].why.is_empty());
    let line = Answer::Queried(view).json_line();
    let json: serde_json::Value = serde_json::from_str(&line).unwrap();
    let first = &json["releases"][0];
    assert_eq!(first["commit"], commit_n(2));
    assert!(first["version"].as_str().unwrap().len() > 3, "{line}");
    assert_eq!(first["why"][0], "current");
}

/// The install, the switch, the confirm and the prune never write the fixed paths of the helper
/// or of the agent, which a running program executes from; no step of the host names a path
/// below `/usr/libexec/kbf` either.
///
/// Catches: an update that writes the helper's or the agent's fixed file.
#[test]
fn an_update_never_writes_the_helper_or_the_agent() {
    let rig = Rig::new("retain-guard");
    let helper = rig
        .root()
        .join(crate::host::SELF_EXE.trim_start_matches('/'));
    let agent = rig.root().join("usr/libexec/kbf/kbf-updater");
    fs::create_dir_all(helper.parent().unwrap()).unwrap();
    fs::write(&helper, b"the helper as it was").unwrap();
    fs::write(&agent, b"the agent as it was").unwrap();
    let marks = [mark(&helper), mark(&agent)];
    let (ab, _) = update_core(&rig, 2);
    for n in 20..24 {
        seed(&rig, &commit_n(n), u64::from(n));
    }
    for n in 1..=3 {
        stage_and_switch(&rig, &ab, n);
        confirm(&ab, n, Some(1));
    }
    assert_eq!([mark(&helper), mark(&agent)], marks, "neither is written");
    let log = format!("{:?}", rig.backend.exec.borrow().log);
    assert!(
        log.contains("/var/lib/kbf-installer"),
        "the log is not empty"
    );
    assert!(!log.contains("/usr/libexec/kbf"), "{log}");
    assert!(dirs(&rig).len() < 3 + 4 + 1, "the prune ran");
}

/// A host upgraded from a release that did not record versions in `retain.json` (its releases on
/// disk, their version text only in this program's own state) gets that text copied there when
/// the socket starts: `host status` and `query` then name each release with the same version.
///
/// Catches: a status that says unknown where the query knows the version; a copy that overwrites
/// a version already recorded.
#[test]
fn an_upgraded_host_has_the_versions_of_its_releases_in_the_status_and_the_query() {
    let rig = Rig::new("retain-upgrade");
    seed(&rig, &commit_n(1), 10);
    seed(&rig, &commit_n(2), 20);
    let (ab, _) = update_core(&rig, 4);
    let state = serde_json::json!({"versions": {
        commit_n(1): version_n(1),
        commit_n(2): version_n(2),
    }});
    let saved = rig.dir.join("state.json");
    fs::write(&saved, state.to_string()).unwrap();
    fs::set_permissions(&saved, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let file = rig.backend.retain_file().unwrap();
    Retained::update(&file, |r| {
        r.versions.insert(id(&commit_n(2)), "recorded".to_owned());
    })
    .unwrap();

    ab.seed_versions();
    let status: Vec<(String, Option<String>)> = kbf_installer::release::kept(&rig.layout)
        .unwrap()
        .into_iter()
        .map(|k| (k.commit.to_string(), k.version))
        .collect();
    let Answer::Queried(view) = ask(&ab, "query") else {
        panic!("a query answers")
    };
    let query: Vec<(String, Option<String>)> = view
        .releases
        .into_iter()
        .map(|k| (k.commit.to_string(), k.version))
        .collect();
    assert_eq!(status, query);
    assert!(
        status.contains(&(commit_n(1), Some(version_n(1)))),
        "{status:?}"
    );
    assert!(
        status.contains(&(commit_n(2), Some("recorded".to_owned()))),
        "{status:?}"
    );
}

/// A confirm whose last good cannot be recorded (the retain file reads, but nothing can be
/// written beside it) removes nothing: the release it names is older than every kept one, and it
/// and every surplus release are still there after the confirm, which stands.
///
/// Catches: a prune that goes on with the last good recorded before, which removes the release
/// the coordinator now names as the one to go back to.
#[test]
fn a_last_good_that_cannot_be_written_removes_nothing() {
    let rig = Rig::new("retain-lg-unwritable");
    for n in 20..28 {
        seed(&rig, &commit_n(n), u64::from(n));
    }
    seed(&rig, &commit_n(1), 1);
    let (ab, notes) = update_core(&rig, 2);
    let file = rig.backend.retain_file().unwrap();
    fs::write(&file, "{}\n").unwrap();
    stage_and_switch(&rig, &ab, 30);
    // What is written beside the file cannot be made: the file reads, and cannot be replaced.
    fs::create_dir(file.with_extension("json.new")).unwrap();
    let before = dirs(&rig);
    let line = format!("confirm kbf={} last_good={}", version_n(30), version_n(1));
    assert_eq!(
        ask(&ab, &line),
        Answer::Confirmed {
            version: version_n(30)
        }
    );
    assert_eq!(dirs(&rig), before, "notes: {:?}", notes.borrow());
    assert!(dirs(&rig).contains(&commit_n(1)));
    let told = notes.borrow();
    assert!(
        told.iter()
            .any(|t| t.starts_with("retain: cannot write") && t.ends_with("; removing nothing")),
        "{told:?}"
    );
}
