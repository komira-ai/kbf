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

fn systemd(unit: &str) -> Restart {
    Restart::Systemd(unit.to_owned())
}

/// A populated lease cgroup under `unit`'s `actions/` below `root`.
fn busy(root: &Path, unit: &str) {
    lease(
        &root.join(SLICE).join(unit).join("actions"),
        "kbf-lease-1-1",
        Some("populated 1\n"),
    );
}

/// Catches a lease source chosen for the wrong unit, or one that does not fail closed. The
/// source of the unit `run` restarts reads that unit's own cgroup: a daemon with a populated
/// lease cgroup has one live lease, one with only an empty (finished) lease cgroup has none,
/// and a unit whose cgroup does not exist cannot tell (the unit never ran, or the path is
/// wrong: neither is "idle").
///
/// Mutants seen red: a missing cgroup read as idle; every unit's source reading
/// `kbf-daemon.service`; the classification narrowed to the daemon's unit (the combined
/// unit is then refused as no mode's).
#[test]
fn the_daemons_lease_source_reads_its_units_cgroup_and_a_missing_one_cannot_tell() {
    let root = scratch_dir("leases-for-daemon");
    busy(&root, "kbf-daemon.service");
    lease(
        &root.join("kbf.slice/kbf-all-in-one.service/actions"),
        "kbf-lease-1-1",
        Some("populated 0\n"),
    );
    let live = |unit: &str| for_restart(&systemd(unit), &root).live();
    assert_eq!(live("kbf-daemon.service"), Ok(1));
    assert_eq!(live("kbf-all-in-one.service"), Ok(0));
    let err = live("kbf-storage-and-daemon.service").expect_err("no such cgroup");
    assert!(err.contains("no such cgroup"), "{err}");
}

/// Catches a mode's unit that the lease source does not know, which would leave its host
/// unable to take an order or, worse, reading another unit's cgroup. Every unit that
/// `setup`'s table can name for any mode is classified: its source reads that unit's own
/// cgroup (a populated lease there is one live lease, another unit's is not counted) and
/// refuses when the cgroup is absent. The table is the one in `setup::plan`; this test
/// breaks, rather than the host, when a mode's unit is renamed or a mode is added.
///
/// Mutants seen red: classification narrowed to the daemon's unit only; a unit's source
/// reading `kbf-daemon.service` for every unit; a missing cgroup read as idle.
#[test]
fn every_unit_a_mode_can_name_is_classified_and_reads_its_own_cgroup() {
    use clap::ValueEnum;
    for mode in crate::cli::Mode::value_variants() {
        let units = crate::setup::plan::mode_units(*mode);
        assert!(!units.is_empty(), "{mode:?} names no unit");
        for unit in units {
            let root = scratch_dir("leases-lockstep");
            busy(&root, unit);
            assert_eq!(
                for_restart(&systemd(unit), &root).live(),
                Ok(1),
                "{mode:?}: {unit}"
            );
            let empty = scratch_dir("leases-lockstep-empty");
            let err = for_restart(&systemd(unit), &empty)
                .live()
                .expect_err("fail closed without the cgroup");
            assert!(err.contains("no such cgroup"), "{unit}: {err}");
        }
    }
}

/// Catches a unit of a mode's own family left out. The package has more than one unit file
/// for a mode: the frontend's handover template runs as any instance (`kbf-frontend@a`,
/// `@b`), the all-in-one has a template too, and the frontend for a host without the proxy is
/// the plain `kbf-frontend.service`. Each reads its own cgroup. Names that are the family's
/// only by prefix are not: another unit of the package, a name with no `.service`, an empty
/// instance and an instance that would leave the slice (a populated cgroup planted at the
/// path it would read) are each refused with the unit named, whatever cgroup exists.
///
/// Mutants seen red (each one makes exactly the units named in the failure wrong): an
/// unknown unit given a source that reads no actions (every one reads idle); the instance's
/// characters not checked (the two instances with a `/` read the planted directories); the
/// empty instance accepted; a prefix match instead of the family's stem (the two names that
/// only start with a family's).
#[test]
fn a_units_family_is_the_modes_and_any_other_unit_is_refused() {
    let root = scratch_dir("leases-family");
    let family = [
        "kbf-frontend.service",
        "kbf-frontend@b.service",
        "kbf-frontend@main-2_x.y:z.service",
        "kbf-all-in-one@1.service",
    ];
    // Every case is judged at once, so a defect shows each unit it gets wrong.
    let live = |unit: &str| for_restart(&systemd(unit), &root).live();
    let read: Vec<_> = family
        .iter()
        .map(|unit| {
            busy(&root, unit);
            (*unit, live(unit))
        })
        .collect();
    let one_each: Vec<_> = family
        .iter()
        .map(|unit| (*unit, Ok::<usize, String>(1)))
        .collect();
    assert_eq!(read, one_each);
    let others = [
        "kbf-installer.service",
        "kbf.service",
        "kbf-frontendx.service",
        "kbf-frontend-b.service",
        "kbf-daemon",
        "kbf-daemon.timer",
        "kbf-frontend@.service",
        "kbf-frontend@a/b.service",
        "kbf-frontend@../b.service",
        "sshd.service",
        "",
    ];
    let refused: Vec<_> = others
        .iter()
        .map(|unit| {
            // The cgroup the unit would read is there and busy, so only the refusal stops it.
            if !unit.is_empty() {
                busy(&root, unit);
            }
            (*unit, live(unit))
        })
        .collect();
    let naming_each: Vec<_> = others
        .iter()
        .map(|unit| {
            let why =
                format!("{unit:?} is no unit of any mode of kbf, so its leases are not known");
            (*unit, Err(why))
        })
        .collect();
    assert_eq!(refused, naming_each);
}

/// Catches a launchd label given a lease source nothing configures: a Mac names no
/// `first_uid` or pool size to the installer, so its source stays the one that cannot tell,
/// and an activation there is refused (fail closed), with the reason.
///
/// Mutant seen red: a label given the cgroup source (the planted cgroup then reads as live).
#[test]
fn a_launchd_label_has_no_lease_source_yet() {
    let root = scratch_dir("leases-launchd");
    busy(&root, "com.komira.kbf-daemon");
    let source = for_restart(&Restart::Launchd("com.komira.kbf-daemon".to_owned()), &root);
    let err = source.live().expect_err("not wired");
    assert!(err.contains("no lease source wired"), "{err}");
}
