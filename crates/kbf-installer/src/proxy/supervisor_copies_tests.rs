//! The cold start of a proxy that keeps its own copies of `kbf` (`--kbf-binary`): the status
//! file is its own, the release is run from its copy, and a start with no record adopts the
//! kbf the package installed. The probes are a [`FakeRunner`] that says the package's kbf is a
//! release build of `A`. The rig of the start is that of `supervisor_tests`.

use std::cell::RefCell;

use super::tests::{Env, gate, state_dir};
use super::*;
use crate::fixtures::{FakeRunner, ran, version_json_full};
use crate::proxy::child_real::Recorded;
use crate::proxy::child_rig::{Rig, rig};
use crate::proxy::releases::{Local, StatusFile};
use crate::testkit::{A, B, c};

/// The package's kbf (some bytes), what `Local` makes of it, and a runner that says it is a
/// build of `A`.
struct Package {
    local: Local,
    runner: FakeRunner,
    status: PathBuf,
}

/// The copies go with the test: they are 0755 files named `kbf`, which the coverage report
/// takes for the binary if they are left under the target directory.
impl Drop for Package {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.local.copies.root());
    }
}

fn package(rig: &Rig) -> Package {
    let state = state_dir(rig);
    let binary = state.join("kbf-package");
    std::fs::write(&binary, b"the package's kbf").expect("binary");
    let stdout = version_json_full(A, "release");
    Package {
        local: Local::open(binary, &state).expect("local"),
        runner: FakeRunner::new(move |_| Ok(ran(0, &stdout, ""))),
        status: state.join("releases.json"),
    }
}

/// The child control of a proxy that runs the releases of the layout.
fn plain(rig: &Rig) -> ProcChild {
    let layout = rig.child.layout.clone();
    ProcChild::new(layout, rig.dir.clone(), Box::new(|_, _| Vec::new()))
}

/// The child control of a proxy that runs its copies.
fn running_copies(rig: &Rig, local: &Local) -> ProcChild {
    run_from_copies(plain(rig), Some(local))
}

fn source<'a>(
    status: &'a Path,
    child: &'a ProcChild,
    adopt: Option<&'a Adopt<'a>>,
) -> HostSource<'a> {
    HostSource {
        status,
        child,
        adopt,
    }
}

/// A migration hook that refuses, and writes down the releases it was asked for.
#[derive(Default)]
struct Asked(RefCell<Vec<CommitId>>);

impl Migrator for Asked {
    fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError> {
        self.0.borrow_mut().push(release.clone());
        Err(PortError("database_error: down".to_owned()))
    }
}

/// Catches a proxy with its own copies that cannot start on a host with no record: the status
/// file has no installed release, the kbf the package installed is copied and recorded as
/// installed, and that copy (not a path in the layout) is the release the boot goes on with:
/// it passes the check for a binary and a marker, and the migration is asked for it. The
/// database refuses, so nothing is spawned.
///
/// Mutants it catches: the adoption left out (the start is held as `no-installed-release`);
/// the child control looking for the release under the layout (`binary-missing`); the
/// adoption run at each attempt (the runner is called more than twice); the record not
/// written (the next attempt adopts again).
#[test]
fn a_cold_start_with_no_record_adopts_the_package_kbf_and_goes_on_with_its_copy() {
    let rig = rig("supervisor-adopt-cold");
    let package = package(&rig);
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    let child = running_copies(&rig, &package.local);
    let asked = Asked::default();
    let boot = Boot {
        migrator: &asked,
        status: &package.status,
        adopt: Some(Adopt {
            local: &package.local,
            runner: &package.runner,
        }),
        ..env.boot()
    };

    let outcome = adopt_children(&gate, &child, &state_dir(&rig), &boot);

    let Startup::Held(why) = outcome else {
        panic!("started on a database that is down");
    };
    assert!(why.contains("migration-failed"), "{why}");
    assert_eq!(*asked.0.borrow(), [c(A), c(A), c(A)]);
    let recorded = StatusFacts::installed(&StatusFile::new(&package.status));
    assert_eq!(recorded, Ok(Some(c(A))));
    assert!(package.local.copies.verified(&c(A)));
    let calls = package.runner.calls();
    assert_eq!(calls.len(), 2, "the package's kbf once, then its copy");
    assert_eq!(calls[0].program, package.local.binary);
    assert_ne!(calls[1].program, package.local.binary);
    assert_eq!(child.recorded(Slot::A), None, "nothing was spawned");
}

/// Catches the children being run from somewhere other than the copies: with the proxy's own
/// copies the releases directory, and the program of a release, are theirs; without them they
/// are the layout's, exactly as before.
///
/// Mutants it catches: `releases_of` that always answers the layout's; `run_from_copies` that
/// returns the child as it was, or that sets the directory without a copy.
#[test]
fn the_children_run_from_the_copies_when_the_proxy_keeps_them_and_from_the_layout_when_not() {
    let rig = rig("supervisor-copies-dirs");
    let package = package(&rig);
    let layout = &rig.child.layout;
    let copies = package.local.copies.root().to_owned();
    assert_eq!(releases_of(Some(&package.local), layout), copies);
    assert_eq!(releases_of(None, layout), layout.join(&layout.releases()));

    let own = run_from_copies(plain(&rig), Some(&package.local));
    assert_eq!(own.program(&c(A)), copies.join(A).join("kbf"));
    let kept = run_from_copies(plain(&rig), None);
    let release = layout.join(&layout.release_dir(&c(A)));
    assert_eq!(kept.program(&c(A)), release.join("kbf"));
}

/// Catches an adoption that happens when it should not, or not when it should: with no record
/// and a way to adopt, the package's kbf is adopted (once: the record it makes is read the
/// next time); with no way to adopt there is nothing installed; a record that is there,
/// another release's, is the answer and the package's kbf is not run; a status file that
/// cannot be read is an error and not an empty one; a kbf that cannot be adopted is an error
/// that says so. The adopted release passes the check for a binary and a marker, and is the
/// copy.
///
/// Mutants it catches: adoption without the match on `None` (a recorded release is replaced,
/// or the runner is called each time); an unreadable file read as no record; the error of the
/// adoption dropped.
#[test]
fn the_installed_release_is_adopted_only_when_none_is_recorded() {
    let rig = rig("supervisor-adopt-source");
    let package = package(&rig);
    let child = running_copies(&rig, &package.local);
    let adopt = Adopt {
        local: &package.local,
        runner: &package.runner,
    };
    let status = package.status.as_path();

    assert_eq!(source(status, &child, None).installed(), Ok(None));
    assert!(package.runner.calls().is_empty(), "nothing to adopt with");

    let adopted = source(status, &child, Some(&adopt));
    assert_eq!(adopted.installed(), Ok(Some(c(A))));
    assert_eq!(package.runner.calls().len(), 2);
    assert_eq!(adopted.installed(), Ok(Some(c(A))));
    assert_eq!(package.runner.calls().len(), 2, "the record is read now");
    assert_eq!(adopted.runnable(&c(A)), Ok(()));
    assert_eq!(child.program(&c(A)), package.local.copies.program(&c(A)));

    std::fs::write(status, format!("{{\"installed_commit\":\"{B}\"}}\n")).expect("record");
    assert_eq!(adopted.installed(), Ok(Some(c(B))));
    assert_eq!(package.runner.calls().len(), 2, "another record stands");

    std::fs::write(status, b"{").expect("unreadable");
    assert!(adopted.installed().is_err());
    assert!(source(status, &child, None).installed().is_err());

    std::fs::remove_file(status).expect("remove");
    let broken = FakeRunner::new(|_| Ok(ran(1, "", "boom")));
    let adopt = Adopt {
        local: &package.local,
        runner: &broken,
    };
    let err = source(status, &child, Some(&adopt))
        .installed()
        .expect_err("the kbf does not run");
    assert!(err.0.starts_with("cannot adopt "), "{}", err.0);
}

/// Catches a prune that is not told about a child still running: the commits are those of the
/// two slots' records (slot A first), a slot with no record or a record that cannot be read adds
/// none; the function the binary watch is given ([`in_use`]) answers the same, from the records
/// as they are when it is called.
///
/// Mutants it catches: one slot left out; the commit read from the wrong field; an unreadable
/// record taken for a commit; the watch's function fixed to the records of its creation.
#[test]
fn the_commits_a_child_may_still_run_are_those_of_the_slots_records() {
    let rig = rig("supervisor-running-commits");
    assert_eq!(running_commits(&rig.child), Vec::<CommitId>::new());
    let kept = in_use(Arc::new(rig.restarted()));
    std::fs::create_dir_all(&rig.dir).expect("children dir");
    let record = |slot: &str, commit: &str| {
        let ident = ProcIdent {
            pid: 1,
            start_time: 1,
            exe: PathBuf::from("/kbf"),
        };
        let recorded = Recorded {
            ident,
            commit: c(commit),
        };
        let bytes = serde_json::to_vec(&recorded).expect("json");
        std::fs::write(rig.dir.join(format!("{slot}.json")), bytes).expect("record");
    };

    record("b", B);
    assert_eq!(running_commits(&rig.child), [c(B)]);
    record("a", A);
    assert_eq!(running_commits(&rig.child), [c(A), c(B)]);
    assert_eq!(kept(), [c(A), c(B)]);
    std::fs::write(rig.dir.join("a.json"), b"{").expect("torn record");
    assert_eq!(running_commits(&rig.child), [c(B)]);
}
