use std::fs;
use std::io;

use super::*;
use crate::fixtures::{FakeRunner, ran, scratch_dir};
use crate::run::Ran;

/// A lease cgroup directory under `actions` with the given `cgroup.events`.
fn lease(actions: &std::path::Path, name: &str, events: Option<&str>) {
    let dir = actions.join(name);
    fs::create_dir_all(&dir).expect("lease dir");
    if let Some(text) = events {
        fs::write(dir.join("cgroup.events"), text).expect("events");
    }
}

/// Catches a lease count that is wrong where it matters: only populated lease cgroups are
/// live (a finished lease's empty cgroup is kept for its OOM events and must not block an
/// update; a populated one holds an action), other directories under `actions/` are not
/// leases, and the answer is read from `cgroup.events` (the container is a child cgroup,
/// so the lease's own process list is empty while its action runs).
///
/// Mutants seen red: `populated 0` counted; the prefix not required; a lease whose action
/// runs in a child counted by its own `cgroup.procs`.
#[test]
fn a_lease_cgroup_is_live_while_it_is_populated() {
    let unit = scratch_dir("leases-cgroup");
    let dir = unit.join("actions");
    lease(&dir, "kbf-lease-1-1", Some("populated 1\nfrozen 0\n"));
    lease(&dir, "kbf-lease-1-2", Some("populated 0\nfrozen 0\n"));
    lease(&dir, "kbf-lease-1-3", Some("populated 1\n"));
    lease(&dir, "supervisor", Some("populated 1\n"));
    fs::write(dir.join("cgroup.procs"), "").expect("file");
    // The lease's own process list is empty: its action is in a child cgroup.
    fs::write(dir.join("kbf-lease-1-1/cgroup.procs"), "").expect("procs");
    assert_eq!(CgroupLeases { unit }.live(), Ok(2));
}

/// Catches a source that cannot tell passing as "none": an unreadable `cgroup.events` of a
/// lease and an `actions` that is not a directory are errors (fail closed). A unit cgroup
/// with no `actions/` is idle (the daemon delegated none), but a unit cgroup that does not
/// exist at all is an error, since a wrong path would otherwise say "no lease is live".
///
/// Mutants seen red: a missing events file counted as not populated; `NotFound` of
/// `actions/` an error even in a unit that exists; `NotFound` with no unit cgroup idle.
#[test]
fn a_cgroup_that_cannot_be_read_is_an_error_and_an_unused_unit_is_idle() {
    let unit = scratch_dir("leases-cgroup-bad");
    lease(&unit.join("actions"), "kbf-lease-1-1", None);
    let err = CgroupLeases { unit: unit.clone() }
        .live()
        .expect_err("events missing");
    assert!(err.contains("cgroup.events"), "{err}");

    let file_unit = unit.join("file-unit");
    fs::create_dir_all(&file_unit).expect("unit");
    fs::write(file_unit.join("actions"), "").expect("file");
    assert!(CgroupLeases { unit: file_unit }.live().is_err());

    let bare = unit.join("bare");
    fs::create_dir_all(&bare).expect("unit");
    assert_eq!(CgroupLeases { unit: bare }.live(), Ok(0));

    let err = CgroupLeases {
        unit: unit.join("absent"),
    }
    .live()
    .expect_err("no such unit cgroup");
    assert!(err.contains("no such cgroup"), "{err}");
}

/// Catches `actions/` read from the wrong place. The daemon delegates it under its own
/// unit's cgroup (`kbf.slice/<unit>/actions`, as the node's own tests lay it out), so each
/// mode's unit names its own, and a lease under a different unit or directly in the slice
/// is not this daemon's.
///
/// Mutants seen red: the unit left out of the path (`kbf.slice/actions`, the earlier
/// default); the slice left out.
#[test]
fn the_actions_cgroup_is_under_the_modes_unit_in_the_slice() {
    let root = scratch_dir("leases-unit");
    let busy = "populated 1\n";
    lease(
        &root.join("kbf.slice/kbf-daemon.service/actions"),
        "kbf-lease-1-1",
        Some(busy),
    );
    lease(
        &root.join("kbf.slice/kbf-all-in-one.service/actions"),
        "kbf-lease-1-1",
        Some(busy),
    );
    lease(
        &root.join("kbf.slice/kbf-all-in-one.service/actions"),
        "kbf-lease-1-2",
        Some(busy),
    );
    lease(&root.join("kbf.slice/actions"), "kbf-lease-9-9", Some(busy));
    let live = |unit: &str| CgroupLeases::of_unit(&root, unit).live();
    assert_eq!(live("kbf-daemon.service"), Ok(1));
    assert_eq!(live("kbf-all-in-one.service"), Ok(2));
    assert!(live("kbf-storage-and-daemon.service").is_err());
    assert_eq!(
        CgroupLeases::of_unit(Path::new(CGROUP_ROOT), "kbf-daemon.service").actions(),
        Path::new("/sys/fs/cgroup/kbf.slice/kbf-daemon.service/actions")
    );
}

fn pool(runner: &FakeRunner) -> PoolLeases<'_> {
    PoolLeases {
        runner,
        first_uid: 600,
        users: 4,
        timeout: Duration::from_secs(5),
    }
}

/// Catches the macOS pool counted wrongly: `ps` is run by absolute path with an empty
/// environment, each pool uid with any process is one live lease however many processes
/// it has, and uids outside `first ..< first + users` (root, a person, the uid just past
/// the pool, the one just below it) are not leases.
///
/// Mutants seen red: processes counted instead of uids; the upper bound inclusive; the
/// lower bound exclusive.
#[test]
fn a_pool_uid_with_a_process_is_one_live_lease() {
    let runner = FakeRunner::new(|_| Ok(ran(0, "  0\n501\n599\n600\n600\n601\n603\n604\n\n", "")));
    assert_eq!(pool(&runner).live(), Ok(3));
    let calls = runner.calls();
    assert_eq!(calls[0].program, PathBuf::from("/bin/ps"));
    assert!(calls[0].env.is_empty());
    assert_eq!(calls[0].timeout, Duration::from_secs(5));
    assert_eq!(calls[0].run_as, None);
}

/// Catches a `ps` that cannot be trusted passing as "none": it would not start, it
/// exited non-zero, it ran out of time, printed what is not a uid or is not text.
#[test]
fn a_ps_that_fails_cannot_tell() {
    let answers: Vec<(&str, io::Result<Ran>)> = vec![
        ("start", Err(io::Error::from(io::ErrorKind::NotFound))),
        ("exit", Ok(ran(1, "", ""))),
        (
            "time",
            Ok(Ran {
                timed_out: true,
                ..ran(0, "", "")
            }),
        ),
        ("number", Ok(ran(0, "root\n", ""))),
        (
            "utf8",
            Ok(Ran {
                stdout: vec![0xff, 0xfe],
                ..ran(0, "", "")
            }),
        ),
    ];
    for (name, answer) in answers {
        let answer = std::cell::RefCell::new(Some(answer));
        let runner = FakeRunner::new(move |_| answer.borrow_mut().take().expect("once"));
        assert!(pool(&runner).live().is_err(), "{name}");
    }
}

/// Catches the not-wired source passing: it cannot tell, so an activation is refused.
#[test]
fn the_unwired_source_cannot_tell() {
    assert!(NotWired.live().is_err());
}
