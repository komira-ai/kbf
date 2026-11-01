//! The Mac backend keeps the last N releases: updates through the core over the kit tree under
//! a scratch directory, with the prune asked for after each confirm.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use kbf_installer::CommitId;
use kbf_installer::retain::Retained;

use super::{Ab, Answer, Backend, Code, LocalRequest, MacBackend, Shared, current, previous, rig};
use crate::fixed_mark::mark;
use crate::mac::tests::fx::{Fx, OLD, OLD_VERSION, Sim, binary, mkdir, write};

/// Seconds after which a release was laid out: every release gets its own time.
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

fn age(fx: &Fx, commit: &str, at: u64) {
    fs::File::open(fx.host.release(commit))
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(BASE + at))
        .unwrap();
}

/// A release directory as an install lays one out, with its version, laid out at `at`.
fn seed(fx: &Fx, commit: &str, version: Option<&str>, at: u64) {
    let dir = fx.host.release(commit);
    mkdir(&dir, 0o755);
    write(&dir.join("kbf"), 0o755, &binary(commit));
    if let Some(version) = version {
        write(
            &dir.join("VERSION"),
            0o644,
            format!("{version}\n").as_bytes(),
        );
    }
    age(fx, commit, at);
}

/// The directories of `releases/` named by a commit.
fn dirs(fx: &Fx) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fx.host.releases())
        .unwrap()
        .map(Result::unwrap)
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

type Notes = Rc<RefCell<Vec<String>>>;

fn core(fx: &Fx, sim: &Rc<Sim>, keep: usize) -> (Ab<MacBackend>, Notes) {
    let notes: Notes = Rc::default();
    let told = Rc::clone(&notes);
    let ab = Ab::new(
        MacBackend::new(fx.host.clone(), Box::new(Shared(Rc::clone(sim)))),
        fx.host.work.join("state.json"),
        fx.host.trust.clone(),
        600,
        Box::new(|| 1_000),
    )
    .keeping(
        keep,
        Box::new(move |text| told.borrow_mut().push(text.to_owned())),
    );
    (ab, notes)
}

fn ask(ab: &Ab<MacBackend>, line: &str) -> Answer {
    ab.handle(&LocalRequest::parse(line).unwrap())
}

fn stage_and_switch(fx: &Fx, ab: &Ab<MacBackend>, n: u32) {
    fx.offer_release(&commit_n(n), &binary(&format!("r{n}")));
    let version = version_n(n);
    let staged = ask(ab, &format!("install kbf={version} from stable"));
    assert!(
        matches!(staged, Answer::Staged { current: false, .. }),
        "{staged:?}"
    );
    age(fx, &commit_n(n), u64::from(n) * 10);
    let switched = ask(ab, &format!("switch kbf={version}"));
    assert!(matches!(switched, Answer::Switched { .. }), "{switched:?}");
}

fn confirm(ab: &Ab<MacBackend>, n: u32, last_good: Option<u32>) {
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

/// After nine updates over a Mac that began on release OLD with a keep of four, what remains is
/// the release that runs, the one before it, the pinned release, the coordinator's last good
/// (named by each confirm from the third on) and the four newest others; the two oldest are
/// gone.
///
/// Catches: a prune that ignores the pin, the last good or the previous release, or keeps one
/// too many or too few.
#[test]
fn after_many_updates_only_the_newest_and_the_ones_that_must_stay_remain() {
    let (fx, sim, backend) = rig("mbr-many");
    age(&fx, OLD, 0);
    let (ab, notes) = core(&fx, &sim, 4);
    for n in 1..=9 {
        stage_and_switch(&fx, &ab, n);
        if n == 1 {
            Retained::update(&backend.retain_file().unwrap(), |r| {
                r.pinned.insert(id(&commit_n(1)));
            })
            .unwrap();
        }
        assert!(
            dirs(&fx).contains(&commit_n(n)),
            "the release on trial is laid out"
        );
        confirm(&ab, n, (n >= 3).then_some(2));
    }
    let mut expected: Vec<String> = [9, 8, 7, 6, 5, 4, 1, 2].map(commit_n).to_vec();
    expected.sort();
    assert_eq!(dirs(&fx), expected, "notes: {:?}", notes.borrow());
    assert_eq!(current(&fx), Some(commit_n(9)));
    assert_eq!(previous(&fx), Some(commit_n(8)));
    assert!(
        notes
            .borrow()
            .iter()
            .any(|t| t.starts_with("prune: keeping: ")),
        "{:?}",
        notes.borrow()
    );
    let file = Retained::read(&backend.retain_file().unwrap()).unwrap();
    assert_eq!(file.last_good, Some(id(&commit_n(2))));
}

/// `retain.json` of a Mac is in root's private work directory.
#[test]
fn the_retain_file_is_in_the_work_directory() {
    let (fx, _, backend) = rig("mbr-place");
    assert_eq!(
        backend.retain_file(),
        Some(fx.host.work.join("retain.json"))
    );
}

/// Nothing is removed while a swap is on trial (the marker is there), whatever the count: the
/// backend's prune says it removes nothing, and the confirm then prunes.
///
/// Catches: a prune while the pending marker is there.
#[test]
fn nothing_is_removed_while_the_marker_is_there() {
    let (fx, sim, backend) = rig("mbr-trial");
    for n in 20..28 {
        seed(&fx, &commit_n(n), Some(&version_n(n)), u64::from(n));
    }
    age(&fx, OLD, 0);
    let (ab, _) = core(&fx, &sim, 2);
    stage_and_switch(&fx, &ab, 1);
    let before = dirs(&fx);
    assert_eq!(before.len(), 10);
    let said = backend.prune(2).unwrap();
    assert_eq!(
        said,
        format!("removing nothing: {} is on trial", commit_n(1))
    );
    assert_eq!(dirs(&fx), before);
    let listed = backend.kept().unwrap();
    let why = |commit: String| {
        listed
            .iter()
            .find(|k| k.commit.to_string() == commit)
            .unwrap()
            .why
            .clone()
    };
    assert_eq!(why(commit_n(1)), ["current", "on_trial"]);
    assert_eq!(why(OLD.to_owned()), ["previous"]);

    confirm(&ab, 1, None);
    let mut expected = vec![commit_n(1), OLD.to_owned(), commit_n(27), commit_n(26)];
    expected.sort();
    assert_eq!(dirs(&fx), expected);
}

/// The listing carries the version of each release from its `VERSION` file, newest first, and
/// why each stays; a release without the file has none.
///
/// Catches: the listing's versions not read from the `VERSION` files.
#[test]
fn the_listing_has_the_versions_from_the_version_files() {
    let (fx, _, backend) = rig("mbr-versions");
    seed(&fx, &commit_n(20), Some(&version_n(20)), 20);
    seed(&fx, &commit_n(21), None, 21);
    age(&fx, OLD, 0);
    let listed: Vec<(String, Option<String>, Vec<&str>)> = backend
        .kept()
        .unwrap()
        .into_iter()
        .map(|k| (k.commit.to_string(), k.version, k.why))
        .collect();
    assert_eq!(
        listed,
        [
            (commit_n(21), None, vec![]),
            (commit_n(20), Some(version_n(20)), vec![]),
            (
                OLD.to_owned(),
                Some(OLD_VERSION.to_owned()),
                vec!["current"]
            ),
        ]
    );
}

/// What is not a release in `releases/` is left alone and not listed: a crashed install's
/// `.<commit>.new`, a stray file, a file named by a commit, a link named by a commit.
#[test]
fn what_is_not_a_release_is_left_alone() {
    let (fx, _, backend) = rig("mbr-strays");
    age(&fx, OLD, 0);
    for n in 20..27 {
        seed(&fx, &commit_n(n), None, u64::from(n));
    }
    let releases = fx.host.releases();
    let aside = releases.join(format!(".{}.new", commit_n(30)));
    mkdir(&aside, 0o755);
    write(&releases.join("notes.txt"), 0o644, b"a note");
    write(
        &releases.join(commit_n(31)),
        0o644,
        b"a file named like a release",
    );
    std::os::unix::fs::symlink(OLD, releases.join(commit_n(32))).unwrap();
    backend.prune(2).unwrap();
    assert!(aside.is_dir() && releases.join("notes.txt").is_file());
    assert!(releases.join(commit_n(31)).is_file());
    assert!(releases.join(commit_n(32)).is_symlink());
    let mut expected = vec![OLD.to_owned(), commit_n(26), commit_n(25)];
    expected.sort();
    assert_eq!(dirs(&fx), expected);
    let listed: Vec<String> = backend
        .kept()
        .unwrap()
        .iter()
        .map(|k| k.commit.to_string())
        .collect();
    assert_eq!(listed, [commit_n(26), commit_n(25), OLD.to_owned()]);
}

/// A count below two keeps two; the pinned release and the last good stay besides.
#[test]
fn a_count_below_two_keeps_two_and_the_pins_stay() {
    let (fx, _, backend) = rig("mbr-min");
    for n in 20..28 {
        seed(&fx, &commit_n(n), None, u64::from(n));
    }
    Retained::update(&backend.retain_file().unwrap(), |r| {
        r.pinned.insert(id(&commit_n(20)));
        r.last_good = Some(id(&commit_n(21)));
    })
    .unwrap();
    let said = backend.prune(0).unwrap();
    assert!(said.contains("and the 2 newest others"), "{said}");
    let mut expected: Vec<String> = [20, 21, 27, 26].map(commit_n).to_vec();
    expected.push(OLD.to_owned());
    expected.sort();
    assert_eq!(dirs(&fx), expected);
}

/// A Mac with no `current` link says nothing is kept by a link ("none"), and a work directory
/// with no `retain.json` is a host with no pins.
#[test]
fn a_kit_with_no_links_keeps_only_the_newest() {
    let (fx, _, backend) = rig("mbr-nolinks");
    fs::remove_file(fx.host.current()).unwrap();
    age(&fx, OLD, 0);
    for n in 20..24 {
        seed(&fx, &commit_n(n), None, u64::from(n));
    }
    let said = backend.prune(2).unwrap();
    assert!(
        said.starts_with("keeping: none and the 2 newest others; removing: "),
        "{said}"
    );
    assert!(said.contains(OLD) && said.contains(&commit_n(20)), "{said}");
    assert_eq!(dirs(&fx), [commit_n(22), commit_n(23)]);
    let said = backend.prune(2).unwrap();
    assert!(said.ends_with("removing: none"), "{said}");
}

/// A listing or a prune that cannot be done is a fault and removes nothing: a link that names no
/// release, a retain file that cannot be parsed (it may hold a pin), a releases directory that
/// is not one, a marker that is damaged, a directory that cannot be written to.
#[test]
fn what_cannot_be_read_or_removed_is_a_fault_and_removes_nothing() {
    let (fx, _, backend) = rig("mbr-faults");
    for n in 20..24 {
        seed(&fx, &commit_n(n), None, u64::from(n));
    }
    let before = dirs(&fx);

    let file = backend.retain_file().unwrap();
    write(&file, 0o600, b"{ not json");
    for fault in [backend.kept().unwrap_err(), backend.prune(2).unwrap_err()] {
        assert_eq!(fault.code, Code::StateUnavailable);
        assert!(
            !fault.refused && fault.detail.contains("cannot be parsed"),
            "{fault:?}"
        );
    }
    assert_eq!(dirs(&fx), before);
    fs::remove_file(&file).unwrap();

    fs::remove_file(fx.host.current()).unwrap();
    std::os::unix::fs::symlink("/somewhere/else", fx.host.current()).unwrap();
    assert_eq!(backend.kept().unwrap_err().code, Code::StateUnavailable);
    assert_eq!(backend.prune(2).unwrap_err().code, Code::StateUnavailable);
    fs::remove_file(fx.host.current()).unwrap();
    std::os::unix::fs::symlink(format!("releases/{OLD}"), fx.host.current()).unwrap();
    assert_eq!(dirs(&fx), before);

    let marker = fx.host.work.join("pending.json");
    write(&marker, 0o600, b"{\"commit\": 7");
    assert_eq!(backend.kept().unwrap_err().code, Code::StateUnavailable);
    // A damaged marker is still a swap on trial: the prune removes nothing.
    assert_eq!(
        backend.prune(2).unwrap(),
        "removing nothing: a release is on trial"
    );
    assert_eq!(dirs(&fx), before);
    fs::remove_file(&marker).unwrap();

    // The entry cannot be removed from a directory that cannot be written to.
    fs::set_permissions(fx.host.releases(), fs::Permissions::from_mode(0o555)).unwrap();
    let fault = backend.prune(2).unwrap_err();
    fs::set_permissions(fx.host.releases(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fault.code, Code::StateUnavailable);
    assert_eq!(dirs(&fx), before);
}

/// A releases directory that is not a directory cannot be listed (a kit that is not there at
/// all is a Mac with no releases).
#[test]
fn a_releases_directory_that_is_not_one_cannot_be_listed() {
    let (fx, _, backend) = rig("mbr-notdir");
    fs::remove_dir_all(fx.host.releases()).unwrap();
    assert!(
        backend.kept().unwrap().is_empty(),
        "no directory, no releases"
    );
    assert!(backend.prune(2).unwrap().contains("removing: none"));
    write(&fx.host.releases(), 0o644, b"a file");
    assert_eq!(backend.kept().unwrap_err().code, Code::StateUnavailable);
    let fault = backend.prune(2).unwrap_err();
    assert_eq!(fault.code, Code::StateUnavailable);
    assert!(fault.detail.contains("releases"), "{fault:?}");
}

/// The install, the switch, the confirm and the prune never write the program's own path (the
/// helper) or the agent's, which a running process executes from.
///
/// Catches: an update that writes the helper's or the agent's fixed file.
#[test]
fn an_update_never_writes_the_helper_or_the_agent() {
    let (fx, sim, _) = rig("mbr-guard");
    let libexec = fx.dir.join("usr/local/libexec/kbf");
    let helper = libexec.join("kbf-host-install");
    let agent = libexec.join("kbf-updater");
    mkdir(&libexec, 0o755);
    write(&helper, 0o755, b"the helper as it was");
    write(&agent, 0o755, b"the agent as it was");
    let marks = [mark(&helper), mark(&agent)];
    let mut host = fx.host.clone();
    host.self_exe = helper.clone();
    let ab = Ab::new(
        MacBackend::new(host, Box::new(Shared(Rc::clone(&sim)))),
        fx.host.work.join("state.json"),
        fx.host.trust.clone(),
        600,
        Box::new(|| 1_000),
    )
    .keeping(2, Box::new(|_| {}));
    for n in 20..24 {
        seed(&fx, &commit_n(n), None, u64::from(n));
    }
    for n in 1..=3 {
        stage_and_switch(&fx, &ab, n);
        confirm(&ab, n, Some(1));
    }
    assert_eq!([mark(&helper), mark(&agent)], marks, "neither is written");
    assert_eq!(fs::read_dir(&libexec).unwrap().count(), 2);
    assert!(dirs(&fx).len() < 3 + 4 + 1, "the prune ran");
}
