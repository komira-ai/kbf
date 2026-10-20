//! Suite 1, NO RESIDUE: after real builds on a node, nothing is left behind.
//!
//! Tier S (this file): the farm's `crun` is the stand-in, so these checks prove what the
//! PRODUCT's own code cleans and keeps (`kbf-daemon`, the container driver, the keeper):
//! lease directories, per-lease crun state (it lives below the lease directory), lease
//! cgroups, the daemon's temporary files, processes of the action's group, disk growth. They
//! do NOT prove that a real container leaves no mount, namespace, kernel cgroup or
//! escaping process behind: see the `#[ignore]`d tier-R tests at the end.
//!
//! What is promised (docs/design/daemon.md, "The container driver"): the lease's container,
//! cgroup and scratch directory are removed and "neither directory may remain", on every path
//! out of a lease; the lease's own `lease_ms` includes removing them, so they are gone when
//! `Execute` returns (asserted at once, never after a settle poll). Kept on purpose, so
//! not residue: the input cache, the trees, the image store, `daemon-instance`
//! (`NodeView::kept`, exact paths, never a prefix).
//!
//! Each check is a function of the probes of `common/probe.rs`; each `planted_` test makes a
//! deliberately leaky action (or switches on a stand-in `crun` knob) and asserts the same
//! probe returns `Err`, so none of them can pass by not looking.

use std::time::Duration;

use super::common::node::NodeView as _;
use super::common::probe::{self, Snapshot};
use super::common::{Job, KillOnDrop, Rig, Tree, noise};

/// The standard planted action: one output file.
const STANDARD: [&str; 3] = ["write-lines", "$UPPER/out/file.txt", "a file"];

fn job(seed: u64) -> Job {
    Job {
        input: Tree::with_input("data.bin", &noise(seed, 4096)),
        outputs: vec!["out/file.txt".to_owned()],
        timeout: None,
        cacheable: false,
    }
}

/// A farm, warmed up: one successful and one failing action have run, so the one-time
/// creations (image store, cache, helper directories, cgroup parents) are in the baseline,
/// not mistaken for residue. Returns the rig and the baseline.
fn warm(name: &str) -> (Rig, Snapshot) {
    let mut rig = Rig::start(name);
    rig.plant(&STANDARD);
    assert_eq!(code(&rig.run(&job(1))), 0);
    rig.plant(&["exit", "3"]);
    assert_eq!(code(&rig.run(&job(2))), 3);
    rig.plant(&STANDARD);
    let base = Snapshot::take(&rig.view()).expect("the baseline");
    assert!(base.leases.is_empty(), "a lease after warm-up: {base:?}");
    assert!(base.pids.is_empty(), "a process after warm-up: {base:?}");
    (rig, base)
}

fn code(response: &kbf_proto::reapi::ExecuteResponse) -> i32 {
    response
        .result
        .as_ref()
        .unwrap_or_else(|| panic!("no result: {response:?}"))
        .exit_code
}

/// The check every test of the file ends in: the node is as `base` was.
fn clean(rig: &mut Rig, base: &Snapshot) -> Result<(), String> {
    probe::clean_since(&rig.view(), base)
}

/// R1-R4, R6: after successful builds there is no lease directory (nor the crun state
/// below it), no lease cgroup, no new path outside the kept ones, no process, no zombie,
/// checked the moment `Execute` returns.
///
/// Catches a lease directory or cgroup the clean forgets, a temporary file the daemon
/// leaves outside the lease, a keeper that outlives its lease, and a clean that runs after
/// the result is reported (the check is immediate).
#[test]
fn successful_runs_leave_nothing() {
    let (mut rig, base) = warm("residue-ok");
    for seed in 10..13 {
        assert_eq!(code(&rig.run(&job(seed))), 0);
        clean(&mut rig, &base).unwrap_or_else(|e| panic!("after run {seed}: {e}"));
    }
}

/// M13, ordering: a lease is cleaned before the next one runs. While run B is alive (parked
/// on a file the test makes), the only lease directory is B's, so A's was gone before B
/// started, and the previous action's output is not in B's.
///
/// Catches a clean that lags behind the result (a late reaper) and a lease directory name
/// that is reused while the old one is still there.
#[test]
fn the_previous_lease_is_gone_before_the_next_one_runs() {
    let (mut rig, base) = warm("residue-order");
    assert_eq!(code(&rig.run(&job(20))), 0);
    let release = rig.dir.join("release");
    rig.plant(&["wait-for", release.to_str().expect("utf-8"), "30"]);
    let b = rig.start_run(&job(21));
    rig.wait_until("B's lease", || !rig.leases().is_empty());
    let leases = rig.leases();
    assert_eq!(leases.len(), 1, "A's lease was still there: {leases:?}");
    let upper = rig
        .data()
        .join("leases")
        .join(leases.first().expect("one"))
        .join("upper");
    // B's upper may hold the directories its outputs go in, and no file.
    let files: Vec<_> = probe::walk(&upper)
        .expect("B's upper")
        .into_iter()
        .filter(|(_, e)| !e.is_dir())
        .collect();
    assert!(
        files.is_empty(),
        "B's upper started with files in it: {files:?}"
    );
    std::fs::write(&release, b"").expect("release");
    assert_eq!(code(&rig.join(b)), 0);
    clean(&mut rig, &base).expect("clean after B");
}

/// R8(a): a failing action (exit 3) leaves nothing; the result carries its exit code.
///
/// Catches a clean that only runs on success.
#[test]
fn a_failing_action_leaves_nothing() {
    let (mut rig, base) = warm("residue-fail");
    rig.plant(&[
        "write-lines",
        "$UPPER/out/file.txt",
        "partial",
        ";",
        "exit",
        "3",
    ]);
    assert_eq!(code(&rig.run(&job(30))), 3);
    clean(&mut rig, &base).expect("clean after a failure");
}

/// R8(b): an action that outlives its timeout is killed and leaves nothing: the lease and
/// cgroup are gone when `Execute` returns, and the sleeping action does not stay.
///
/// Catches a timeout path that skips the clean or the group kill.
#[test]
fn a_timed_out_action_leaves_nothing() {
    let (mut rig, base) = warm("residue-timeout");
    rig.plant(&["sleep", "120"]);
    let mut timed = job(40);
    timed.timeout = Some(Duration::from_secs(2));
    let response = rig.run(&timed);
    let status = response.status.as_ref().expect("a status");
    assert_eq!(status.code, 4, "not a deadline: {status:?}");
    clean(&mut rig, &base).expect("clean after a timeout");
}

/// R4: the action's children in its process group are killed with the lease (the group
/// kill `cgroup.kill` is, on the stand-in). The action starts a child that sleeps ten
/// minutes and goes on; after the lease no process of the farm is left.
///
/// Catches a lease that ends without killing the group, and a keeper that is not reaped.
#[test]
fn children_in_the_actions_group_are_killed() {
    let (mut rig, base) = warm("residue-children");
    rig.plant(&["spawn-child", "--pid", "-", "sleep", "600"]);
    let response = rig.run(&job(50));
    assert_eq!(code(&response), 0);
    assert_eq!(rig.stdout_of(&response).lines().count(), 1, "one child pid");
    clean(&mut rig, &base).expect("clean after a child");
}

/// R7: disk. Six runs, each with its own 1 MiB input and 1 MiB output. After each, the node's
/// scratch directory outside the kept ones is exactly as it was (no new path, no byte more:
/// slack zero, so no leak can hide in it); the input cache grows by at most the new input
/// and the content stores (CAS) by at most what the run was given and made; and the SAME
/// input again adds nothing to the cache. `statvfs` is not used: it is global to the host.
///
/// Catches a lease that leaves bytes outside the kept paths, a cache that stores an
/// input twice, and a CAS that keeps more than an action's inputs, outputs and logs.
#[test]
fn runs_do_not_grow_the_node_beyond_what_it_keeps() {
    const MIB: usize = 1 << 20;
    let (mut rig, base) = warm("residue-disk");
    rig.plant(&["upper", "$UPPER/out/file.txt", "$ROOT/in/data.bin"]);
    let big = |seed| Job {
        input: Tree::with_input("data.bin", &noise(seed, MIB)),
        outputs: vec!["out/file.txt".to_owned()],
        timeout: None,
        cacheable: false,
    };
    let mut before = base.clone();
    for seed in 100..106 {
        assert_eq!(code(&rig.run(&big(seed))), 0);
        let now = Snapshot::take(&rig.view()).expect("snapshot");
        clean(&mut rig, &base).unwrap_or_else(|e| panic!("run {seed}: {e}"));
        let cache = now.kept.saturating_sub(before.kept);
        let stores = now.stores.1.saturating_sub(before.stores.1);
        eprintln!("run {seed}: cache +{cache} B, stores +{stores} B (apparent)");
        growth_within(cache, stores, MIB as u64).unwrap_or_else(|e| panic!("run {seed}: {e}"));
        before = now;
    }
    // The same input again: a cache hit adds no bytes to the cache.
    let again = big(100);
    assert_eq!(code(&rig.run(&again)), 0);
    let now = Snapshot::take(&rig.view()).expect("snapshot");
    assert!(
        now.kept <= before.kept + 64 * 1024,
        "the cache grew for an input it holds: {} -> {}",
        before.kept,
        now.kept
    );
}

/// What one run may add: its input once to the cache (plus directory blocks), and to the
/// stores its input and its output once each, plus half a MiB of logs and index (measured:
/// 2 MiB and about 100 KB for a 1 MiB input and output; this farm has one storage node, so
/// one copy of a blob).
fn growth_within(cache: u64, stores: u64, input: u64) -> Result<(), String> {
    let cache_max = input + 256 * 1024;
    let stores_max = 2 * input + (512 << 10);
    if cache > cache_max {
        return Err(format!("the cache grew {cache} B for a {input} B input"));
    }
    if stores > stores_max {
        return Err(format!("the stores grew {stores} B for a {input} B input"));
    }
    Ok(())
}

/// R8(d): the daemon is killed (SIGKILL) while an action runs, and restarted. The
/// start-up sweep removes the old lease's directory and cgroup, and the lease that is
/// run again gets a name of its own (the old name is not reused). On the stand-in the
/// action process itself outlives the daemon (the keeper writes `cgroup.kill`, a plain file
/// the stand-in does not act on): that is the stand-in's limit, so the action here is
/// released and the process check is made after it ended; the real kill is tier R.
///
/// Catches a sweep that leaves the dead daemon's lease, a rerun that reuses the old lease
/// name (and so its half-written stdout), and state in `runs/` or `quarantine/`.
#[test]
fn a_restarted_daemon_sweeps_the_lease_it_left() {
    use kbf_harness::farm::Role;
    let (mut rig, base) = warm("residue-restart");
    let release = rig.dir.join("release");
    rig.plant(&["wait-for", release.to_str().expect("utf-8"), "60"]);
    let run = rig.start_run(&job(60));
    rig.wait_until("the lease", || !rig.leases().is_empty());
    let old = rig.leases();
    rig.farm.kill(Role::Node(1));
    rig.plant(&STANDARD);
    // The keeper of the dead daemon writes `cgroup.kill` and ends; only then is the planted
    // cgroup tree settled.
    let node = rig.view();
    rig.wait_until("the keeper to end", || {
        node.processes()
            .is_ok_and(|p| p.iter().all(|p| !p.cmd.contains("__keeper")))
    });
    stand_in_rmdir(&rig);
    rig.farm.restart(Role::Node(1));
    rig.farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    // The sweep ran as part of the start: the old lease is gone before anything is asked.
    let after = rig.leases();
    for name in &old {
        assert!(
            !after.contains(name),
            "the old lease {name} survived the sweep"
        );
    }
    std::fs::write(&release, b"").expect("release");
    assert_eq!(code(&rig.join(run)), 0);
    clean(&mut rig, &base).expect("clean after the restart");
    // A fresh run after the restart: its lease is new, and nothing of the old ones is back.
    assert_eq!(code(&rig.run(&job(61))), 0);
    clean(&mut rig, &base).expect("clean after the next run");
}

/// What the stand-in's cgroup tree lacks: on a real cgroup v2 mount `rmdir` removes a
/// cgroup together with its interface files (`memory.max`, `cgroup.kill`), here plain files.
/// The start-up sweep writes `cgroup.kill` in a dead lease's cgroup before it removes it,
/// which on plain directories makes the directory non-empty (ENOTEMPTY) and the daemon
/// refuse to start. That is the stand-in's doing, not the product's, so the dead lease's
/// cgroup directory is removed whole, as the cgroup would be once a kernel had nothing in it;
/// the sweep then finds it gone (which it accepts). The sweep's removal of a LIVE cgroup is
/// tier R.
fn stand_in_rmdir(rig: &Rig) {
    let actions = rig
        .bin()
        .join("cgroup/kbf.slice/kbf-all-in-one.service/actions");
    for entry in probe::read_dir(&actions).expect("the actions cgroup") {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(probe::LEASE_PREFIX) {
            for file in probe::read_dir(&entry.path()).expect("lease cgroup") {
                std::fs::remove_file(file.path()).expect("an interface file");
            }
            std::fs::remove_dir(entry.path()).expect("the lease cgroup");
        }
    }
}

// ---- planted leaks: the same probes, shown red -------------------------------------------

/// The residue a planted action leaves, as the probe reports it.
fn planted(name: &str, steps: &[&str]) -> (Rig, Result<(), String>) {
    let (mut rig, base) = warm(name);
    rig.plant(steps);
    // The action may fail on purpose; only its residue matters here.
    let _ = rig.run(&job(70));
    let verdict = clean(&mut rig, &base);
    (rig, verdict)
}

fn data_path(rig: &Rig, rel: &str) -> String {
    rig.data().join(rel).to_str().expect("utf-8").to_owned()
}

/// R1 red: an action leaves a lease directory in the node's lease area.
#[test]
fn planted_lease_directory_is_seen() {
    let (mut rig, base) = warm("plant-lease");
    let stray = data_path(&rig, "leases/kbf-lease-9-9");
    rig.plant(&["mkdir", &stray]);
    let _ = rig.run(&job(71));
    let seen = clean(&mut rig, &base).expect_err("a lease directory was left");
    assert!(
        seen.contains("lease directory left: kbf-lease-9-9"),
        "{seen}"
    );
}

/// R3 red, the real way: `crun delete` fails, so the product turns the lease into a farm
/// fault ("a dirty node must be loud", infra failure after the reruns) and the lease cgroup
/// directories stay. The probe must see them; the lease directories, which the daemon
/// removes itself, are gone.
#[test]
fn planted_failing_delete_leaves_cgroups_that_are_seen() {
    let (mut rig, base) = warm("plant-delete");
    rig.knob("delete-fails", "");
    let response = rig.run(&job(72));
    let status = response.status.as_ref().expect("a status");
    assert_eq!(status.code, 13, "the dirty node was not loud: {status:?}");
    let seen = clean(&mut rig, &base).expect_err("cgroups were left");
    assert!(seen.contains("lease cgroup left:"), "{seen}");
}

/// R3 red, direct: an action makes a lease-named cgroup beside its own (`$CG/..` is the
/// `actions` directory). It survives the lease's end, which is why this plant is valid.
#[test]
fn planted_cgroup_directory_is_seen() {
    let (_rig, seen) = planted("plant-cgroup", &["mkdir", "$CG/../kbf-lease-9-9"]);
    let seen = seen.expect_err("a cgroup was left");
    assert!(seen.contains("kbf-lease-9-9"), "{seen}");
}

/// R4 red: the action starts a child in a session of its own (`setsid`), outside the group
/// the stand-in's `cgroup.kill` ends. This is the stand-in's known limit (a real cgroup
/// reaches it; tier R), so the plant proves the probe sees an escapee, and the test ends it.
#[test]
fn planted_escaping_process_is_seen() {
    let (mut rig, base) = warm("plant-escapee");
    rig.plant(&["spawn-child", "--pid", "-", "setsid", ";;", "sleep", "601"]);
    let response = rig.run(&job(73));
    let mut reaper = KillOnDrop::default();
    reaper.0.extend(
        rig.stdout_of(&response)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok()),
    );
    let seen = clean(&mut rig, &base).expect_err("a process escaped");
    assert!(seen.contains("process left:"), "{seen}");
}

/// R6 red: an action leaves a file in the node's scratch directory outside the lease.
#[test]
fn planted_stray_file_is_seen() {
    let stray = "stray.tmp";
    let (mut rig, base) = warm("plant-stray");
    let path = data_path(&rig, stray);
    rig.plant(&["write-lines", &path, "x"]);
    let _ = rig.run(&job(74));
    let seen = clean(&mut rig, &base).expect_err("a file was left");
    assert!(seen.contains("new path: stray.tmp"), "{seen}");
}

/// R7 red: a file that existed before the run grows (a log, a counter, a database that
/// should not): a path in the baseline is not an excuse.
#[test]
fn planted_growth_of_an_existing_file_is_seen() {
    let (mut rig, _) = warm("plant-growth");
    let path = data_path(&rig, "leases/ledger");
    std::fs::write(&path, b"").expect("a file in the baseline");
    let base = Snapshot::take(&rig.view()).expect("baseline");
    rig.plant(&["append", &path, "more", "bytes"]);
    let _ = rig.run(&job(75));
    let seen = clean(&mut rig, &base).expect_err("the file grew");
    assert!(seen.contains("grew: leases/ledger"), "{seen}");
}

/// S1 red: the allow-list is exact. Names that merely start with a kept name are residue:
/// `cache-evil`, `trees-x`, `leases/oci-images.bak`, `leases/daemon-instance2`.
#[test]
fn planted_lookalikes_of_kept_paths_are_seen() {
    let (mut rig, base) = warm("plant-lookalike");
    let names = [
        "cache-evil",
        "trees-x",
        "leases/oci-images.bak",
        "leases/daemon-instance2",
    ];
    let mut steps: Vec<String> = Vec::new();
    for name in names {
        steps.extend([
            "write-lines".to_owned(),
            data_path(&rig, name),
            "x".to_owned(),
            ";".to_owned(),
        ]);
    }
    steps.pop();
    let refs: Vec<&str> = steps.iter().map(String::as_str).collect();
    rig.plant(&refs);
    let _ = rig.run(&job(76));
    let seen = clean(&mut rig, &base).expect_err("lookalikes were left");
    for name in names {
        assert!(
            seen.contains(&format!("new path: {name}")),
            "{name} not seen: {seen}"
        );
    }
}

/// M14 red: a record in `runs/` and a file in `quarantine/` (the native driver's keeper
/// records and quarantine, daemon.md "The start-up sweep") are residue on this node too:
/// neither directory exists in the baseline and neither may appear.
#[test]
fn planted_runs_and_quarantine_entries_are_seen() {
    let (mut rig, base) = warm("plant-runs");
    let (runs, quarantine) = (data_path(&rig, "runs"), data_path(&rig, "quarantine"));
    let (record, moved) = (
        format!("{runs}/lease-1-1"),
        format!("{quarantine}/lease-1-1"),
    );
    rig.plant(&[
        "mkdir",
        &runs,
        ";",
        "write-lines",
        &record,
        "x",
        ";",
        "mkdir",
        &quarantine,
        ";",
        "write-lines",
        &moved,
        "x",
    ]);
    let _ = rig.run(&job(77));
    let seen = clean(&mut rig, &base).expect_err("records were left");
    for path in ["runs/lease-1-1", "quarantine/lease-1-1"] {
        assert!(
            seen.contains(&format!("new path: {path}")),
            "{path}: {seen}"
        );
    }
}

/// R7 red: an action writes a file of 4 MiB outside the lease; the growth is seen by size
/// (apparent), not only as a new name.
#[test]
fn planted_large_stray_file_is_seen_with_its_size() {
    let (mut rig, base) = warm("plant-big");
    let path = data_path(&rig, "leases/spill");
    rig.plant(&["truncate", &path, "4194304"]);
    let _ = rig.run(&job(78));
    let now = Snapshot::take(&rig.view()).expect("snapshot");
    let entry = now
        .files
        .get(std::path::Path::new("leases/spill"))
        .expect("the spill");
    assert_eq!(entry.len, 4 << 20);
    assert!(clean(&mut rig, &base).is_err());
}

/// R7 red: the stores' growth bound is not vacuous. An action whose stdout is 24 MiB
/// makes the CAS keep far more than its 1 MiB input and output can explain.
#[test]
fn planted_runaway_store_growth_is_seen() {
    const MIB: u64 = 1 << 20;
    let (mut rig, _) = warm("plant-store");
    let before = Snapshot::take(&rig.view()).expect("snapshot");
    rig.plant(&["zeros", "25165824"]);
    let _ = rig.run(&job(79));
    let now = Snapshot::take(&rig.view()).expect("snapshot");
    let grew = now.stores.1.saturating_sub(before.stores.1);
    assert!(
        grew >= 24 * MIB,
        "the stores did not keep the stdout: +{grew}"
    );
    assert!(growth_within(0, grew, MIB).is_err());
}

/// M5 red: a directory the user cannot read (mode 000, as the stand-in makes an upper
/// directory) inside the scratch directory makes the probe fail, not skip it.
#[test]
fn planted_unreadable_directory_fails_the_probe() {
    let (mut rig, base) = warm("plant-unreadable");
    let locked = data_path(&rig, "leases/locked");
    rig.plant(&["mkdir", &locked, ";", "chmod", "0", &locked]);
    let _ = rig.run(&job(80));
    // A file inside would have been invisible to a walk that skipped the directory.
    let seen = Snapshot::take(&rig.view());
    let reason = seen.expect_err("an unreadable directory was skipped");
    assert!(reason.contains("Permission denied"), "{reason}");
    // The same probe through the checker used by every test.
    let failed = clean(&mut rig, &base).expect_err("clean must not pass");
    assert!(failed.contains("cannot look"), "{failed}");
    // Leave the directory removable.
    std::fs::set_permissions(
        rig.data().join("leases/locked"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod");
}

/// A missing lease area is an error, never an empty one: a probe on a wrong path must not
/// pass (M5). No farm needed: the node is pointed at a directory that is not one.
#[test]
fn planted_missing_lease_area_fails_the_probe() {
    let scratch = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "no-such-farm");
    let nowhere = scratch.path().join("nowhere");
    let wrong = super::common::node::LinuxNode::new(&nowhere, Vec::new());
    assert!(wrong.lease_dirs().is_err());
    assert!(Snapshot::take(&wrong).is_err());
}

/// R4 red: a zombie is residue. A child of the test that exited and was not waited for is
/// a zombie of its parent; the zombie probe finds it by state and parent.
#[test]
fn planted_zombie_is_seen() {
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let pid = child.id();
    // Let it exit without reaping it.
    let me = std::process::id();
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let found = loop {
        let zombies = probe::zombie_children(&[me]).expect("scan");
        if zombies.iter().any(|z| z.pid == pid) {
            break true;
        }
        assert!(std::time::Instant::now() < end, "no zombie appeared");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(found);
    assert!(!probe::alive(pid).expect("alive"), "a zombie is not alive");
    child.wait().expect("reap");
    assert!(
        probe::zombie_children(&[me])
            .expect("scan")
            .iter()
            .all(|z| z.pid != pid),
        "the reaped child is still listed"
    );
}

/// R8 red, after a restart: a stray file made before the daemon is killed is not something
/// the sweep removes, and the post-restart check names it.
#[test]
fn planted_stray_file_survives_a_restart_and_is_seen() {
    use kbf_harness::farm::Role;
    let (mut rig, base) = warm("plant-restart");
    let stray = data_path(&rig, "left-before-crash");
    rig.plant(&["write-lines", &stray, "x"]);
    let _ = rig.run(&job(81));
    rig.farm.kill(Role::Node(1));
    rig.farm.restart(Role::Node(1));
    rig.farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let seen = clean(&mut rig, &base).expect_err("the stray file stayed");
    assert!(seen.contains("new path: left-before-crash"), "{seen}");
}

// ---- tier R: needs real rootless crun; fails loudly when run on the stand-in --------------
//
// ONE lease on real `crun` is already covered by kbf-driver-container's tests/oci_crun.rs
// (`the_lease_ends_with_the_action_whatever_it_left_running`, `timeout_and_kill_leave_nothing_behind`,
// `an_orphan_is_reaped_so_a_killed_group_is_gone`, run by `cargo xtask crun-tests`). What
// the tests below add, once a farm runs on real crun, is the node-level residue after
// MANY leases through the product's whole path (the probes of this file over a
// `NodeView` of the real cgroup tree), not another single-lease check.

/// What a real-`crun` leg must prove and the stand-in cannot. Run with the repo's
/// `cargo xtask crun-tests` job (a `Delegate=yes` unit with `crun` and `uidmap`), once the
/// full farm runs on real crun (komira-ai/kbf#936) or a daemon-only harness drives the
/// container driver: the SAME probes, `NodeView` implemented over the real cgroup tree.
fn real_crun_needed(what: &str) -> ! {
    panic!(
        "tier R: {what} needs real rootless crun, user and mount namespaces and a delegated \
         cgroup v2 tree; the stand-in has none, and a pass here would prove nothing"
    )
}

/// R5: no container (`crun list` empty), no overlay or bind mount of the lease left in
/// `/proc/<daemon>/mountinfo`, no kernel cgroup directory left by a killed `crun run`.
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_no_container_mount_or_kernel_cgroup_is_left() {
    real_crun_needed("the mount and cgroup residue of a real container")
}

/// R4: an escapee (setsid) from the action is killed by the lease cgroup's `cgroup.kill`
/// (daemon.md "The keeper"), and the keeper kills the group when the daemon is killed.
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_a_real_cgroup_kill_reaches_escapees_and_a_killed_daemons_actions() {
    real_crun_needed("cgroup.kill reaching a setsid escapee and a SIGKILLed daemon's action")
}

/// R6/S5: `/tmp`, `/var/tmp`, `/run`, `/root` and the 64 MiB `/dev/shm` of an action are the
/// lease's own, so writing them leaves nothing on the host (the stand-in's are the host's).
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_temp_and_shared_memory_are_the_leases_own() {
    real_crun_needed("the container's private /tmp and /dev/shm")
}
