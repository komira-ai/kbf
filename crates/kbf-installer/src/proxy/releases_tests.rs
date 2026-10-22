//! The proxy's own copies of `kbf` and its local order port ([`super::releases`]). The probe
//! is a [`FakeRunner`] that says what a build prints for `--version --json`, so a test sees
//! the copy as the probe saw it; the status file is read back through the real [`StatusFile`].
//!
//! Each "Mutants it catches" line names a change to the code that the test is written to turn
//! red. They are the reason the test exists, not a record that anyone ran them.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::model::{Locator, Source, StagedFacts, Verb};
use super::ports::{RootOrders, StatusFacts};
use super::releases::{Copies, CopyError, Local, LocalOrders, RELEASES, STATUS};
use super::root_orders::StatusFile;
use crate::CommitId;
use crate::fixtures::{FakeRunner, ran, scratch_dir, version_json, version_json_full};
use crate::testkit::{A, B, C, c};

type Made = Result<PathBuf, CopyError>;

/// A scratch directory with a copies root in it (`releases/`) and the sources the tests copy.
struct Rig {
    dir: PathBuf,
    releases: PathBuf,
    copies: Arc<Copies>,
}

fn rig(name: &str) -> Rig {
    let dir = scratch_dir(&format!("releases-{name}"));
    let releases = dir.join("releases");
    let copies = Arc::new(Copies::new(&releases).expect("copies"));
    Rig {
        dir,
        releases,
        copies,
    }
}

/// The scratch directory goes with the test: a 0755 file named `kbf` left under the target
/// directory is taken for the `kbf` binary by the coverage report, which then fails on it.
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// A source file of `bytes`, 0644: a copy must not take that mode over.
    fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.join(name);
        write(&path, bytes);
        chmod(&path, 0o644);
        path
    }

    fn ensure(&self, runner: &FakeRunner, src: &Path, commit: &str) -> Made {
        self.copies.ensure(runner, src, &c(commit))
    }

    /// A verified copy of `commit`, whose build says what it is.
    fn copy(&self, commit: &str) {
        let src = self.source(&format!("src-{commit}"), commit.as_bytes());
        let runner = says(commit, "release");
        self.ensure(&runner, &src, commit).expect("copy");
    }
}

fn write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write");
}

fn chmod(path: &Path, mode: u32) {
    let permissions = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, permissions).expect("chmod");
}

fn mkdir(path: &Path) {
    fs::create_dir_all(path).expect("mkdir");
}

fn mode(path: &Path) -> u32 {
    let permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.mode() & 0o777
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let entries = fs::read_dir(dir).expect("list");
    let mut names: Vec<String> = entries
        .map(|entry| entry.expect("entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A runner whose program prints the version of a `profile` build of `commit`.
fn says(commit: &str, profile: &str) -> FakeRunner {
    let stdout = version_json(commit, profile);
    FakeRunner::new(move |_| Ok(ran(0, &stdout, "")))
}

/// What the probe saw of the program it was asked to run.
struct Seen {
    program: PathBuf,
    mode: u32,
    marker: bool,
    placed: bool,
}

/// Catches a copy that is not the source (a short read, a missing chunk), a mode that follows
/// the source, the probe running the source instead of the copy, and the marker or the final
/// directory existing before the probe has passed.
///
/// Mutants it catches: the marker written before the probe; the rename done before the probe;
/// the probe run on `src`; the copy loop dropping its last chunk; the copy taking the mode of
/// a source of 0644.
#[test]
fn a_copy_is_byte_identical_executable_and_marked_only_after_its_probe() {
    let rig = rig("copy");
    let bytes: Vec<u8> = (0..200_000u32)
        .map(|i| u8::try_from(i % 251).expect("fits"))
        .collect();
    let src = rig.source("kbf-src", &bytes);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&seen);
    let releases = rig.releases.clone();
    let stdout = version_json(A, "release");
    let runner = FakeRunner::new(move |spawn| {
        let dir = spawn.program.parent().expect("in a directory");
        log.borrow_mut().push(Seen {
            program: spawn.program.clone(),
            mode: mode(&spawn.program),
            marker: dir.join("VERIFIED").exists(),
            placed: releases.join(A).exists(),
        });
        Ok(ran(0, &stdout, ""))
    });

    let program = rig.ensure(&runner, &src, A).expect("ensure");

    assert_eq!(program, rig.releases.join(A).join("kbf"));
    assert_eq!(program, rig.copies.program(&c(A)));
    assert_eq!(fs::read(&program).expect("copy"), bytes);
    assert_eq!(mode(&program), 0o755);
    assert_eq!(mode(&src), 0o644);
    let marker = program.with_file_name("VERIFIED");
    let sum = hex::encode(Sha256::digest(&bytes));
    let text = fs::read_to_string(&marker).expect("marker");
    assert_eq!(text, format!("sha256 {sum}\n"));
    assert_eq!(mode(&marker), 0o644);
    assert_eq!(names(&rig.releases), [A]);
    assert_eq!(names(&rig.releases.join(A)), ["VERIFIED", "kbf"]);
    assert!(rig.copies.verified(&c(A)));

    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    let probe = &seen[0];
    assert_ne!(probe.program, src);
    assert!(probe.program.to_string_lossy().contains(".partial-"));
    assert_eq!(probe.mode, 0o755);
    assert!(!probe.marker, "marker before the probe");
    assert!(!probe.placed, "placed before the probe");
}

/// Catches a copy that stays when its probe fails (a binary that does not run, or that is
/// not a `kbf`, would then be a release the proxy starts children from).
///
/// Mutants it catches: the rename done before the probe; the probe's failure ignored; the
/// partial directory left behind on an error; the migrator's own refusal word carried into
/// the text.
#[test]
fn a_failing_probe_leaves_no_release_directory() {
    let rig = rig("probe");
    let src = rig.source("kbf-src", b"not a kbf");
    let runner = FakeRunner::new(|_| Ok(ran(1, "", "boom")));

    let err = rig.ensure(&runner, &src, A).expect_err("the probe fails");

    assert!(matches!(err, CopyError::ProbeFailed(_)), "{err}");
    let want = "probe_failed: --version --json did not finish cleanly";
    assert!(err.to_string().starts_with(want));
    assert!(names(&rig.releases).is_empty());
    assert!(!rig.copies.verified(&c(A)));
}

/// Catches a binary being accepted for a commit it is not: another commit's build, or a
/// debug build of the right one.
///
/// Mutants it catches: the `check` of the version dropped; only the commit compared (not the
/// profile).
#[test]
fn a_binary_that_reports_another_commit_or_a_debug_build_is_refused() {
    let rig = rig("mismatch");
    let src = rig.source("kbf-src", b"another build");
    for (reported, profile) in [(B, "release"), (A, "debug")] {
        let runner = says(reported, profile);

        let err = rig.ensure(&runner, &src, A).expect_err("refused");

        assert!(matches!(err, CopyError::CommitMismatch(_)), "{err}");
        assert!(err.to_string().starts_with("commit_mismatch: "));
        assert!(names(&rig.releases).is_empty());
    }
}

/// Catches a second copy of a commit replacing the first, and a repeat doing the work again:
/// the same bytes are the same release (nothing is run), other bytes are refused and change
/// nothing.
///
/// Mutants it catches: overwrite (copy over the existing directory); the existing directory
/// removed and rebuilt; the probe run again for the same bytes; `differs` not returned.
#[test]
fn the_same_bytes_are_idempotent_and_other_bytes_are_refused() {
    let rig = rig("again");
    let first = rig.source("first", b"the first bytes");
    let other = rig.source("other", b"other bytes");
    let runner = says(A, "release");
    let program = rig.ensure(&runner, &first, A).expect("first");
    let marker = program.with_file_name("VERIFIED");
    let before = fs::read(&marker).expect("marker");

    let silent = FakeRunner::new(|_| Err(io::Error::other("must not run")));
    let again = rig.ensure(&silent, &first, A).expect("the same bytes");
    assert_eq!(again, program);
    assert!(silent.calls().is_empty(), "it ran");

    let err = rig.ensure(&silent, &other, A).expect_err("other bytes");
    assert!(matches!(err, CopyError::Differs(_)), "{err}");
    assert!(err.to_string().starts_with("differs: "));
    assert_eq!(fs::read(&program).expect("copy"), b"the first bytes");
    assert_eq!(fs::read(&marker).expect("marker"), before);
    assert!(silent.calls().is_empty(), "it ran");
    assert_eq!(names(&rig.releases), [A]);
}

/// Catches a directory being trusted by its name: without its marker, with a marker for other
/// bytes, or with a binary that is gone or is no longer the one that was probed, it is not the
/// release of the source in hand.
///
/// Mutants it catches: the marker not required; the marker's sum not compared; the binary not
/// hashed again; the two compared with `||` instead of both required.
#[test]
fn a_directory_without_its_marker_or_its_bytes_is_not_the_release() {
    let cases: [(&str, Option<&[u8]>); 4] = [
        ("VERIFIED", None),
        ("VERIFIED", Some(b"sha256 00\n")),
        ("kbf", Some(b"changed")),
        ("kbf", None),
    ];
    for (i, (file, put)) in cases.into_iter().enumerate() {
        let rig = rig(&format!("tamper-{i}"));
        let src = rig.source("kbf-src", b"genuine");
        let runner = says(A, "release");
        rig.ensure(&runner, &src, A).expect("first");
        let path = rig.releases.join(A).join(file);
        match put {
            Some(bytes) => write(&path, bytes),
            None => fs::remove_file(&path).expect("remove"),
        }

        let err = rig.ensure(&runner, &src, A).expect_err("not the release");

        assert!(matches!(err, CopyError::Differs(_)), "{file}: {err}");
    }
}

/// Catches an unreadable source being a success, or leaving its half-made copy: a source that
/// does not exist, one that opens and then fails to read (a directory), and the same unread
/// source for a commit that has a copy already.
///
/// Mutants it catches: the partial directory not removed on an error; a read error ending the
/// copy as if it were the end of the file; the open's error dropped.
#[test]
fn a_source_that_cannot_be_read_is_copy_failed_and_leaves_nothing() {
    let rig = rig("unreadable");
    let runner = says(A, "release");

    let missing = rig.dir.join("absent");
    let err = rig.ensure(&runner, &missing, A).expect_err("missing");
    assert!(matches!(err, CopyError::CopyFailed(_)), "{err}");
    assert!(err.to_string().starts_with("copy_failed: cannot open "));

    let err = rig.ensure(&runner, &rig.dir, A).expect_err("a directory");
    assert!(matches!(err, CopyError::CopyFailed(_)), "{err}");
    assert!(err.to_string().contains("cannot copy to "));
    assert!(names(&rig.releases).is_empty());
    assert!(runner.calls().is_empty(), "it ran");

    let src = rig.source("kbf-src", b"bytes");
    rig.ensure(&runner, &src, A).expect("a real source");
    let err = rig.ensure(&runner, &rig.dir, A).expect_err("a directory");
    assert!(matches!(err, CopyError::CopyFailed(_)), "{err}");
    assert!(err.to_string().contains("cannot read "));
    assert_eq!(names(&rig.releases), [A]);
}

/// Catches a copy that loses the race for its directory (another one was put in place while
/// this one was probed) replacing the other, or leaving its partial behind.
///
/// Mutants it catches: the rename's error ignored; the partial not removed after a failed
/// rename; the other copy's files removed first.
#[test]
fn a_copy_that_cannot_be_put_in_place_is_copy_failed_and_leaves_no_partial() {
    let rig = rig("race");
    let src = rig.source("kbf-src", b"bytes");
    let stdout = version_json(A, "release");
    let taken = rig.releases.join(A);
    let runner = FakeRunner::new(move |_| {
        fs::create_dir(&taken).expect("another copy arrives");
        write(&taken.join("kbf"), b"theirs");
        Ok(ran(0, &stdout, ""))
    });

    let err = rig.ensure(&runner, &src, A).expect_err("lost the race");

    assert!(matches!(err, CopyError::CopyFailed(_)), "{err}");
    assert!(err.to_string().contains("cannot install "));
    assert_eq!(names(&rig.releases), [A]);
    let binary = rig.releases.join(A).join("kbf");
    assert_eq!(fs::read(binary).expect("theirs"), b"theirs");
}

/// Catches a root that cannot be written to being reported as anything but `copy_failed`
/// (CI runs the tests as an ordinary user, to whom a 0555 directory is closed).
///
/// Mutants it catches: the creation of the partial directory not checked; the probe run
/// without a copy.
#[test]
fn a_root_that_cannot_be_written_is_copy_failed() {
    let rig = rig("readonly");
    let src = rig.source("kbf-src", b"bytes");
    let runner = says(A, "release");
    chmod(&rig.releases, 0o555);

    let err = rig.ensure(&runner, &src, A).expect_err("a closed root");

    chmod(&rig.releases, 0o755);
    assert!(matches!(err, CopyError::CopyFailed(_)), "{err}");
    assert!(err.to_string().contains("cannot make "));
    assert!(runner.calls().is_empty(), "it ran");
}

/// Catches a leftover of a killed copy staying (it would pile up, and a name that is reused
/// would refuse), and the sweep removing more than that: releases and strangers stay.
///
/// Mutants it catches: no sweep; a sweep of every entry; a sweep of directories only (the
/// leftover file stays); a sweep of files only.
#[test]
fn new_sweeps_what_a_killed_copy_left_and_nothing_else() {
    let dir = scratch_dir("releases-sweep");
    let root = dir.join("releases");
    mkdir(&root.join(".partial-1-0"));
    write(&root.join(".partial-1-0").join("kbf"), b"half");
    write(&root.join(".partial-2-7"), b"a file");
    mkdir(&root.join(A));
    write(&root.join(A).join("kbf"), b"kept");
    write(&root.join("notes"), b"kept");

    let copies = Copies::new(&root).expect("new");

    assert_eq!(names(&root), [A, "notes"]);
    assert_eq!(fs::read(copies.program(&c(A))).expect("kept"), b"kept");
    fs::remove_dir_all(&dir).expect("the scratch directory goes");
}

/// Catches a root that is made with the wrong mode or not made at all (the proxy's state
/// directory has no `releases/` until the first copy), and a root that cannot be made being
/// taken for one.
///
/// Mutants it catches: no creation; a non-recursive creation; a mode other than 0755.
#[test]
fn new_makes_the_root_and_refuses_one_that_cannot_be_made() {
    let dir = scratch_dir("releases-new");
    let root = dir.join("deeper").join("releases");

    Copies::new(&root).expect("new");

    assert!(root.is_dir());
    assert_eq!(mode(&root), 0o755);
    Copies::new(&root).expect("again, with the root there");
    write(&dir.join("file"), b"x");
    assert!(Copies::new(dir.join("file")).is_err());
}

/// A status file as the root half writes it: the proxy's three keys, and a `state` word that
/// the proxy never writes.
const ROOT_HALF: &str = concat!(
    "{\"installed_commit\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
    "\"staged_commit\":\"0123456789abcdef0123456789abcdef01234567\",",
    "\"state\":\"staged\",\"staged\":{\"protocol\":4,\"min_protocol\":3,\"state_schema\":7}}",
);

struct Orders {
    rig: Rig,
    status_file: PathBuf,
    orders: LocalOrders,
}

fn orders(name: &str) -> Orders {
    let rig = rig(name);
    let status_file = rig.dir.join("releases.json");
    let orders = LocalOrders::new(&status_file, Arc::clone(&rig.copies));
    Orders {
        rig,
        status_file,
        orders,
    }
}

fn facts(commit: &str) -> StagedFacts {
    StagedFacts {
        commit: c(commit),
        protocol: 4,
        min_protocol: 3,
        state_schema: 7,
        profile: Some("release".to_owned()),
    }
}

/// The staged facts and the installed commit, as every reader of `status.json` reads the file.
fn read(file: &Path) -> (Option<StagedFacts>, Option<CommitId>) {
    let reader = StatusFile::new(file);
    let staged = StatusFacts::staged(&reader).expect("staged");
    let installed = StatusFacts::installed(&reader).expect("installed");
    (staged, installed)
}

/// Catches the three orders and the two reads disagreeing with the real [`StatusFile`] about
/// the file: what `stage` writes reads back as the facts and commit it was given, `confirm`
/// moves it to installed, `revert` drops it and leaves the installed one.
///
/// Mutants it catches: `confirm` that does not clear `staged`; `confirm` that does not set
/// `installed`; `stage` that forgets the installed commit; `revert` that clears `installed`;
/// `staged_commit` written as another field.
#[test]
fn stage_confirm_and_revert_round_trip_through_the_status_file() {
    let o = orders("trip");
    o.rig.copy(A);
    o.rig.copy(B);
    assert_eq!(read(&o.status_file), (None, None));

    o.orders.stage(&facts(A)).expect("stage A");
    assert_eq!(read(&o.status_file), (Some(facts(A)), None));
    o.orders.confirm(&c(A)).expect("confirm A");
    assert_eq!(read(&o.status_file), (None, Some(c(A))));

    o.orders.stage(&facts(B)).expect("stage B");
    assert_eq!(read(&o.status_file), (Some(facts(B)), Some(c(A))));
    o.orders.revert(&c(B)).expect("revert B");
    assert_eq!(read(&o.status_file), (None, Some(c(A))));
}

/// Catches `confirm` landing a release that was never staged (the swap would then call a
/// release installed that no probe ever saw), and a second `confirm` of the same release
/// (a restart between the confirm and its record sends it again) failing or rewriting the
/// file (a rewrite is a rename, so the file would get another inode).
///
/// Mutants it catches: `confirm` without clearing `staged`; `confirm` that accepts any
/// commit; `confirm` that is an error the second time; `confirm` that rewrites the file when
/// the release is installed already.
#[test]
fn confirm_of_a_release_that_is_not_staged_is_refused_and_twice_is_fine() {
    let o = orders("confirm");
    o.rig.copy(A);
    o.rig.copy(B);
    assert!(o.orders.confirm(&c(A)).is_err(), "nothing staged yet");
    o.orders.stage(&facts(A)).expect("stage");

    let err = o.orders.confirm(&c(B)).expect_err("B is not staged");

    assert_eq!(err.0, format!("{} is not the staged release", c(B)));
    assert_eq!(read(&o.status_file), (Some(facts(A)), None));
    o.orders.confirm(&c(A)).expect("first");
    assert_eq!(read(&o.status_file), (None, Some(c(A))));
    let landed = fs::metadata(&o.status_file).expect("meta").ino();
    o.orders.confirm(&c(A)).expect("again");
    assert_eq!(fs::metadata(&o.status_file).expect("meta").ino(), landed);
    assert_eq!(read(&o.status_file), (None, Some(c(A))));
    assert!(o.orders.confirm(&c(C)).is_err(), "never staged");
}

/// Catches `revert` failing when nothing is staged (the swap reverts a release that never got
/// staged), writing a file for nothing, and dropping the staging of a release other than the
/// one it was asked about.
///
/// Mutants it catches: `revert` that is an error with nothing staged; `revert` that clears
/// `staged` whatever the commit; `revert` that writes the file when it changes nothing.
#[test]
fn revert_is_ok_with_nothing_staged_and_leaves_another_release_staged() {
    let o = orders("revert");
    o.rig.copy(A);
    o.orders.revert(&c(A)).expect("nothing staged");
    assert!(!o.status_file.exists(), "a file for nothing");
    o.orders.stage(&facts(A)).expect("stage");

    o.orders.revert(&c(C)).expect("another release");

    assert_eq!(read(&o.status_file), (Some(facts(A)), None));
    o.orders.revert(&c(A)).expect("revert");
    assert_eq!(read(&o.status_file), (None, None));
    o.orders.revert(&c(A)).expect("again");
}

/// Catches `stage` recording a release the proxy has no verified copy of (the swap would then
/// start a child from a file that is not there): no copy at all, a binary without its marker
/// and a marker without its binary.
///
/// Mutants it catches: no check; only the directory checked; only the binary checked; only
/// the marker checked.
#[test]
fn stage_needs_the_copy_and_its_marker() {
    let o = orders("nocopy");
    let err = o.orders.stage(&facts(A)).expect_err("no copy");
    assert_eq!(err.0, format!("no verified copy of {}", c(A)));
    o.rig.copy(B);
    o.rig.copy(C);
    let marker = o.rig.releases.join(B).join("VERIFIED");
    let binary = o.rig.releases.join(C).join("kbf");
    fs::remove_file(marker).expect("remove");
    fs::remove_file(binary).expect("remove");

    assert!(o.orders.stage(&facts(B)).is_err(), "no marker");
    assert!(o.orders.stage(&facts(C)).is_err(), "no binary");

    assert!(!o.status_file.exists(), "a file for nothing");
    o.rig.copy(A);
    o.orders.stage(&facts(A)).expect("a verified copy");
}

/// Catches `staged` and `installed` not reading the file the root half's reader reads: a file
/// in the shape of `status.json` (with a word the proxy does not write) is read as its facts.
///
/// Mutants it catches: `staged` that is always `None`; `installed` that reads `staged_commit`;
/// `staged` that reads the commit from `installed_commit`.
#[test]
fn staged_and_installed_read_the_shape_the_root_half_writes() {
    let o = orders("shape");
    write(&o.status_file, ROOT_HALF.as_bytes());
    let staged = StagedFacts {
        commit: c(B),
        protocol: 4,
        min_protocol: 3,
        state_schema: 7,
        profile: None,
    };

    assert_eq!(RootOrders::staged(&o.orders), Ok(Some(staged)));
    assert_eq!(RootOrders::installed(&o.orders), Ok(Some(c(A))));
}

/// Catches the file written in a shape another reader cannot read, with a mode other than
/// 0644, or with its temporary file left beside it.
///
/// Mutants it catches: a key renamed; `staged` nested differently; the facts' profile left
/// out; the file written with another mode; the rename skipped.
#[test]
fn the_file_has_the_shape_of_status_json_and_mode_0644() {
    let o = orders("file");
    o.rig.copy(A);
    o.rig.copy(B);
    o.orders.stage(&facts(A)).expect("stage A");
    o.orders.confirm(&c(A)).expect("confirm A");
    o.orders.stage(&facts(B)).expect("stage B");

    let text = fs::read_to_string(&o.status_file).expect("file");

    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["installed_commit"], A);
    assert_eq!(value["staged_commit"], B);
    assert_eq!(value["staged"]["protocol"], 4);
    assert_eq!(value["staged"]["min_protocol"], 3);
    assert_eq!(value["staged"]["state_schema"], 7);
    assert_eq!(value["staged"]["profile"], "release");
    assert_eq!(mode(&o.status_file), 0o644);
    assert!(!o.status_file.with_extension("json.new").exists());
    o.orders.revert(&c(B)).expect("revert B");
    let text = fs::read_to_string(&o.status_file).expect("file");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert!(value["staged_commit"].is_null());
    assert!(value["staged"].is_null());
}

/// Catches `frontend_update` doing anything: nothing is spooled and no file is written.
///
/// Mutant it catches: `frontend_update` that returns `Ok`.
#[test]
fn frontend_update_is_refused() {
    let o = orders("frontend");
    let locator = Locator {
        order_id: 1,
        epoch: 1,
        generation: 1,
        verb: Verb::FrontendUpdate,
        commit: c(A),
        window_s: 900,
        source: Source::Fetch,
    };

    let err = o.orders.frontend_update(&locator).expect_err("refused");

    assert_eq!(err.0, "frontend_update is not an order here");
    assert!(!o.status_file.exists());
}

/// Catches a status file that cannot be read being taken for an empty one (the next write
/// would erase what it said) and a file that cannot be written being taken for written.
///
/// Mutants it catches: a parse error read as `None`; the write's error dropped; the file
/// replaced without being read first.
#[test]
fn an_unreadable_file_is_an_error_and_is_not_overwritten_and_a_failed_write_is_one_too() {
    let o = orders("corrupt");
    o.rig.copy(A);
    write(&o.status_file, b"{");

    assert!(o.orders.stage(&facts(A)).is_err());
    assert!(o.orders.confirm(&c(A)).is_err());
    assert!(o.orders.revert(&c(A)).is_err());

    assert_eq!(fs::read(&o.status_file).expect("file"), b"{");
    let file = o.rig.dir.join("absent").join("releases.json");
    let orders = LocalOrders::new(file, Arc::clone(&o.rig.copies));
    let err = orders.stage(&facts(A)).expect_err("no directory");
    assert!(err.0.starts_with("cannot write "), "{err}");
}

/// Catches an adoption that replaces a release that is recorded, drops what is staged, writes
/// the file when it has nothing to change, or lets a release without a copy be called
/// installed: with `B` staged and nothing installed `A` is adopted and `B` stays staged; a
/// second adoption (of another release) answers `A` and leaves the file as it is; a commit
/// with no verified copy, and a file that cannot be read, are errors that change nothing.
///
/// Asserts that `adopt` keeps a recorded installed release; that it writes the staged facts
/// with the adoption; that it leaves the file alone when a release is recorded; that it
/// checks for the copy; and that an unreadable file is an error and not an empty record.
#[test]
fn adopt_records_the_installed_release_only_when_none_is_recorded() {
    let o = orders("adopt");
    let err = o.orders.adopt(&c(A)).expect_err("no copy");
    assert_eq!(err.0, format!("no verified copy of {}", c(A)));
    assert!(!o.status_file.exists(), "a file for nothing");
    o.rig.copy(A);
    o.rig.copy(B);
    o.orders.stage(&facts(B)).expect("stage B");

    let adopted = o.orders.adopt(&c(A)).expect("adopt A");

    assert_eq!(adopted, c(A));
    assert_eq!(read(&o.status_file), (Some(facts(B)), Some(c(A))));
    let landed = fs::metadata(&o.status_file).expect("meta").ino();
    let other = o.orders.adopt(&c(B));
    let again = other.expect("another release is not adopted");
    assert_eq!(again, c(A));
    assert_eq!(fs::metadata(&o.status_file).expect("meta").ino(), landed);
    assert_eq!(read(&o.status_file), (Some(facts(B)), Some(c(A))));

    write(&o.status_file, b"{");
    assert!(o.orders.adopt(&c(A)).is_err());
    assert_eq!(fs::read(&o.status_file).expect("file"), b"{");
}

/// A runner whose program prints the version of a `release` build of `commit`, with its full
/// commit (`commit_full`).
fn says_full(commit: &str) -> FakeRunner {
    let stdout = version_json_full(commit, "release");
    FakeRunner::new(move |_| Ok(ran(0, &stdout, "")))
}

/// Catches the copies and the orders of a `Local` not being where the proxy looks for them:
/// `releases/` and `releases.json` in the state directory, the kbf it was told, and a root
/// that cannot be made refused.
///
/// Asserts that the copies are under `releases/` and not in the state directory itself; that
/// the status file is beside the copies and not under them; that the binary is kept; and that
/// the root's error is returned.
#[test]
fn a_local_keeps_its_copies_and_its_orders_in_the_state_directory() {
    let rig = rig("local-open");
    let state = rig.dir.join("state");
    mkdir(&state);
    let binary = rig.dir.join("kbf-package");

    let local = Local::open(binary.clone(), &state).expect("open");

    assert_eq!(local.binary, binary);
    assert_eq!(local.copies.root(), state.join(RELEASES));
    assert!(state.join(RELEASES).is_dir());
    let src = rig.source("src", b"bytes");
    let runner = says(A, "release");
    local.copies.ensure(&runner, &src, &c(A)).expect("a copy");
    local.orders.adopt(&c(A)).expect("adopt");
    assert_eq!(read(&state.join(STATUS)), (None, Some(c(A))));
    write(&state.join("file"), b"x");
    assert!(Local::open(binary, &state.join("file")).is_err());
}

/// Catches the package's kbf being adopted wrongly: it is probed where it is, copied under the
/// full commit it names, probed again as a copy, recorded as installed, and a repeat does not
/// copy or record again (it asks the package's kbf and no more).
///
/// Asserts that the copy is named by the full commit and not the short one; that the record
/// is written; that a repeat makes no second copy; and that the package's kbf is probed.
#[test]
fn the_package_kbf_is_copied_under_its_full_commit_and_recorded_as_installed() {
    let rig = rig("local-adopt");
    let state = rig.dir.join("state");
    mkdir(&state);
    let binary = rig.source("kbf-package", b"the package's kbf");
    let local = Local::open(binary.clone(), &state).expect("open");
    let runner = says_full(A);

    let adopted = local.adopt(&runner).expect("adopt");

    assert_eq!(adopted, c(A));
    let copy = fs::read(local.copies.program(&c(A))).expect("copy");
    assert_eq!(copy, b"the package's kbf");
    assert_eq!(read(&state.join(STATUS)), (None, Some(c(A))));
    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].program, binary);
    assert_ne!(calls[1].program, binary);
    let again = local.adopt(&runner).expect("adopt again");
    assert_eq!(again, c(A));
    assert_eq!(runner.calls().len(), 3, "only the package's kbf is asked");
}

/// Catches an adoption that goes on when the package's kbf cannot be vouched for, and an
/// error that does not say which kbf: one that does not run, one that prints no full commit (a
/// build before the field, or without a checkout), one that prints something that is not a
/// commit, one whose copy is another build, and a record that cannot be written. Each is an
/// error that starts with the path and, but for the last, leaves no copy and no record.
///
/// Asserts that a probe failure stops the adoption; that a missing `commit_full` is an error
/// and is not read as the short commit; that the full commit is checked for its shape; that
/// the copy is probed; and that the record's error is returned.
#[test]
fn a_kbf_that_cannot_be_named_or_copied_is_not_adopted() {
    let rig = rig("local-adopt-refused");
    let state = rig.dir.join("state");
    mkdir(&state);
    let binary = rig.source("kbf-package", b"bytes");
    let local = Local::open(binary.clone(), &state).expect("open");
    let prefix = format!("cannot adopt {}: ", binary.display());
    let broken = FakeRunner::new(|_| Ok(ran(1, "", "boom")));
    let short = says(A, "release");
    let odd = version_json(A, "release").replacen('}', ",\"commit_full\":\"ABC\"}", 1);
    let odd = FakeRunner::new(move |_| Ok(ran(0, &odd, "")));
    let source_says = version_json_full(A, "release");
    let copy_says = version_json(B, "release");
    let program = binary.clone();
    let lying = FakeRunner::new(move |spawn| {
        let said = if spawn.program == program {
            &source_says
        } else {
            &copy_says
        };
        Ok(ran(0, said, ""))
    });

    let cases = [
        (
            &broken,
            "probe_failed: --version --json did not finish cleanly",
        ),
        (&short, "it prints no commit_full"),
        (&odd, "\"ABC\" is not a commit id"),
        (&lying, "commit_mismatch: "),
    ];
    for (runner, want) in cases {
        let err = local.adopt(runner).expect_err(want);
        assert!(err.0.starts_with(&prefix), "{}", err.0);
        assert!(err.0[prefix.len()..].starts_with(want), "{}", err.0);
        assert!(names(&state.join(RELEASES)).is_empty(), "{}", err.0);
        assert!(!state.join(STATUS).exists(), "{}", err.0);
    }

    let honest = says_full(A);
    chmod(&state, 0o555);
    let err = local.adopt(&honest).expect_err("a closed state directory");
    chmod(&state, 0o755);
    assert!(err.0.starts_with("cannot write "), "{}", err.0);
}
