//! Suite 2, NO CROSS-ACTION DATA ACCESS ("layout" tier): action A's data is not reachable by
//! action B on the same node, B after A.
//!
//! READ THIS BEFORE TRUSTING A GREEN RUN. On the stand-in `crun` all actions are plain
//! processes of one user in one mount, pid and network namespace, so a green run here proves
//! the LAYOUT the product builds and the paths it collects and cleans, never that a
//! container cannot read its neighbour. What each check can and cannot show is in its doc;
//! the isolation checks proper (B concurrent with A reads A's live files; uid, mode,
//! mount, pid and network namespaces; copy-up of the input links; the container's
//! environment) are the `#[ignore]`d tier-R tests at the end, which fail loudly on the
//! stand-in. Cross-action protection on a Linux node is "B cannot name A's files" (a mount
//! namespace per lease), not "B is another user": all actions are the same container uid
//! (daemon.md "User namespaces"); a Mac leg is per-uid (`Isolation::PerUid`).
//!
//! Why B only runs after A here: the stand-in keeps ONE record of the last action
//! (`pid`, `root`, `upper`, `owner`) for the whole node, so two leases in flight would
//! signal and delete each other's process. Concurrent A and B is tier R.
//!
//! Canaries are 16 KiB of unpredictable hex (an incompressible, unmistakable needle). A's
//! canaries sit in its input root, its output, its upper directory, and the lease's `/tmp`
//! and `/var/tmp` directories (`rw/tmp`, `rw/var-tmp`); the check scans the whole farm
//! directory, outside the content stores (which hold bytes by digest by design), for them.

use kbf_proto::reapi::ExecuteResponse;
use kbf_transfer::digest_of;

use super::common::node::{Isolation, NodeView as _};
use super::common::probe;
use super::common::{Job, KillOnDrop, Rig, Tree, canary};

const CANARY_LEN: usize = 16 * 1024;

fn code(response: &ExecuteResponse) -> i32 {
    response.result.as_ref().expect("a result").exit_code
}

fn text(path: &std::path::Path) -> String {
    path.to_str().expect("utf-8").to_owned()
}

/// A's script: print where it is, then write its canary to every place it can: output,
/// upper, and the lease's temp directories. `extra` are more places, for the planted leaks.
fn steps_a(extra: &[String]) -> Vec<String> {
    let c = canary(1, CANARY_LEN);
    let mut s: Vec<String> = Vec::new();
    let mut add = |words: &[&str]| {
        s.extend(words.iter().map(|w| (*w).to_owned()));
        s.push(";".to_owned());
    };
    add(&["env", "ROOT"]);
    add(&["env", "UPPER"]);
    add(&["copy", "$UPPER/out/canary.out", "$ROOT/in/canary.in"]);
    add(&["write-lines", "$UPPER/canary.upper", &c]);
    add(&["write-lines", "$ROOT/../rw/tmp/canary.tmp", &c]);
    add(&["write-lines", "$ROOT/../rw/var-tmp/canary.tmp", &c]);
    for e in extra {
        s.push(e.clone());
    }
    if s.last().is_some_and(|l| l == ";") {
        s.pop();
    }
    s
}

fn job_a() -> Job {
    Job {
        input: Tree::with_input("canary.in", canary(1, CANARY_LEN).as_bytes()),
        outputs: vec!["out/canary.out".to_owned()],
        ..Job::default()
    }
}

fn job_b() -> Job {
    Job {
        input: Tree::with_input("canary.in", canary(2, CANARY_LEN).as_bytes()),
        outputs: vec!["out/file.txt".to_owned()],
        ..Job::default()
    }
}

/// B's script: exits 0 only if none of `a_paths` exists for it, its own upper holds none of
/// A's files, and its input root holds B's bytes.
fn steps_b(a_paths: &[String]) -> Vec<String> {
    let mut s: Vec<String> = Vec::new();
    let mut add = |words: &[&str]| {
        s.extend(words.iter().map(|w| (*w).to_owned()));
        s.push(";".to_owned());
    };
    for p in a_paths {
        add(&["test", "!", "-e", p]);
    }
    add(&["test", "!", "-e", "$UPPER/canary.upper"]);
    add(&["test", "!", "-e", "$UPPER/out/canary.out"]);
    add(&["test", "!", "-e", "$ROOT/../rw/tmp/canary.tmp"]);
    add(&["test", "!", "-e", "$ROOT/../rw/var-tmp/canary.tmp"]);
    add(&["holds", "$ROOT/in/canary.in", &canary(2, CANARY_LEN)]);
    add(&["write-lines", "$UPPER/out/file.txt", "b"]);
    s.pop();
    s
}

fn plant(rig: &Rig, steps: &[String]) {
    let refs: Vec<&str> = steps.iter().map(String::as_str).collect();
    rig.plant(&refs);
}

/// A's lease, and the places it wrote its canaries in, from its stdout (`ROOT`, `UPPER`).
fn a_paths(stdout: &str) -> Vec<String> {
    let mut lines = stdout.lines();
    let root = lines.next().expect("A printed ROOT").to_owned();
    let upper = lines.next().expect("A printed UPPER").to_owned();
    vec![
        format!("{root}/in/canary.in"),
        format!("{upper}/out/canary.out"),
        format!("{upper}/canary.upper"),
        format!("{root}/../rw/tmp/canary.tmp"),
        format!("{root}/../rw/var-tmp/canary.tmp"),
        format!("{root}/.."),
    ]
}

/// The verdict on B after A: B ran clean (it could not name A's data), and A's canary is in
/// no file of the farm outside the content stores and in nothing B returned.
fn b_saw_nothing(rig: &mut Rig, b: &ExecuteResponse) -> Result<(), String> {
    let needle = canary(1, CANARY_LEN);
    let mut bad = Vec::new();
    if code(b) != 0 {
        bad.push(format!(
            "B could see A's data (its checks exited {}): {}",
            code(b),
            rig.stdout_of(b).lines().next().unwrap_or("")
        ));
    }
    let hits = probe::files_containing(&rig.view(), &rig.dir, needle.as_bytes())?;
    for hit in hits {
        let rel = hit.strip_prefix(&rig.dir).unwrap_or(&hit);
        bad.push(format!("A's canary is in {}", rel.display()));
    }
    if rig.stdout_of(b).contains(&needle) {
        bad.push("A's canary is in B's stdout".to_owned());
    }
    for file in &b.result.as_ref().expect("a result").output_files {
        let bytes = rig
            .blob(file.digest.as_ref().expect("digest"))
            .unwrap_or_default();
        if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
            bad.push(format!("A's canary is in B's output {}", file.path));
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join("; "))
    }
}

/// Runs A then B on the node; B's verdict.
fn a_then_b(rig: &mut Rig, extra: &[String], extra_paths: &[String]) -> Result<(), String> {
    plant(rig, &steps_a(extra));
    let a = rig.run(&job_a());
    assert_eq!(code(&a), 0, "A failed: {a:?}");
    let mut paths = a_paths(&rig.stdout_of(&a));
    paths.extend(extra_paths.iter().cloned());
    plant(rig, &steps_b(&paths));
    let b = rig.run(&job_b());
    b_saw_nothing(rig, &b)
}

/// C2 (sequential), X1-X3 at the layout level: A writes its canary into its input root,
/// output, upper, `/tmp` and `/var/tmp` and ends; B, on the same node, cannot name any of
/// A's paths (not even A's lease directory), starts with an empty upper and `/tmp`, and
/// holds its own input bytes; and A's canary is in no file of the farm outside the content
/// stores: not a log, not a leftover scratch file, not B's stdout, stderr or outputs.
///
/// Catches a lease that is not cleaned before the next one (or whose directories are
/// reused with their contents), a daemon that copies an action's data to a shared place
/// (a log, a scratch directory), and an upper or temp directory that starts non-empty.
/// It cannot catch a runtime that lets B read A's LIVE files: that is tier R.
#[test]
fn a_later_action_finds_nothing_of_an_earlier_one() {
    let mut rig = Rig::start("cross-sequential");
    // The stand-in isolates nothing; a node that does is judged by the tier-R tests.
    assert_eq!(rig.view().isolation(), Isolation::Nothing);
    a_then_b(&mut rig, &[], &[]).unwrap_or_else(|e| panic!("{e}"));
}

/// Red for the check above: A also leaves a copy of its canary in a shared directory
/// outside its lease. B's path check and the farm scan must both name it.
#[test]
fn planted_shared_file_between_actions_is_seen() {
    let mut rig = Rig::start("cross-plant-shared");
    let shared = text(&rig.dir.join("shared"));
    let file = format!("{shared}/canary");
    let extra = steps_extra_write(&shared, &file);
    let seen = a_then_b(&mut rig, &extra, &[file]).expect_err("the shared copy was missed");
    assert!(seen.contains("B could see A's data"), "{seen}");
    assert!(seen.contains("A's canary is in shared/canary"), "{seen}");
}

fn steps_extra_write(dir: &str, file: &str) -> Vec<String> {
    let c = canary(1, CANARY_LEN);
    ["mkdir", dir, ";", "write-lines", file, &c]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

/// M6, the output collector: an output that is a symlink to another action's (or the host's)
/// file is reported as a symlink, never followed, and an output whose parent is a symlink
/// is refused; the target's bytes reach no output and not the CAS (daemon.md "Outputs":
/// opened by descriptor, no symlink followed). Product code, runs unchanged on the stand-in.
///
/// Catches a collector that follows a link the action planted and uploads what it points at.
#[test]
fn outputs_that_are_links_are_not_followed() {
    let rig = Rig::start("cross-output-links");
    let secret_dir = rig.dir.join("secret");
    std::fs::create_dir(&secret_dir).expect("mkdir");
    let secret = canary(3, CANARY_LEN);
    std::fs::write(secret_dir.join("canary"), &secret).expect("secret");
    let target = text(&secret_dir.join("canary"));
    // (a) the output itself is a link to the secret file.
    rig.plant(&["symlink", &target, "$UPPER/out/x"]);
    let direct = rig.run(&Job {
        outputs: vec!["out/x".to_owned()],
        ..Job::default()
    });
    output_escape(&rig, &direct, &secret).unwrap_or_else(|e| panic!("direct link: {e}"));
    let links = &direct.result.as_ref().expect("result").output_symlinks;
    assert_eq!(
        links.len(),
        1,
        "the link was not reported as a link: {direct:?}"
    );
    // (b) the output's parent is a link to the directory holding the secret.
    rig.plant(&["symlink", &text(&secret_dir), "$UPPER/o2"]);
    let parent = rig.run(&Job {
        outputs: vec!["o2/canary".to_owned()],
        ..Job::default()
    });
    output_escape(&rig, &parent, &secret).unwrap_or_else(|e| panic!("parent link: {e}"));
}

/// The secret reached an output or the CAS.
fn output_escape(rig: &Rig, response: &ExecuteResponse, secret: &str) -> Result<(), String> {
    let result = response.result.as_ref().ok_or("no result")?;
    for file in &result.output_files {
        let digest = file.digest.as_ref().ok_or("no digest")?;
        let bytes = rig.blob(digest).unwrap_or_default();
        if *digest == digest_of(secret.as_bytes())
            || bytes.windows(secret.len()).any(|w| w == secret.as_bytes())
        {
            return Err(format!("output {} holds the secret's bytes", file.path));
        }
    }
    if rig.blob(&digest_of(secret.as_bytes())).is_some() {
        return Err("the secret's blob is in the CAS".to_owned());
    }
    Ok(())
}

/// Red for `output_escape`: an action that copies the secret into a declared output (it
/// can, on the stand-in) hands the probe a result that holds it.
#[test]
fn planted_output_with_a_neighbours_bytes_is_seen() {
    let rig = Rig::start("cross-plant-output");
    let secret = canary(3, CANARY_LEN);
    let path = rig.dir.join("secret-file");
    std::fs::write(&path, &secret).expect("secret");
    rig.plant(&["copy", "$UPPER/out/x", &text(&path)]);
    let leaked = rig.run(&Job {
        outputs: vec!["out/x".to_owned()],
        ..Job::default()
    });
    let seen = output_escape(&rig, &leaked, &secret).expect_err("the copy was missed");
    assert!(seen.contains("holds the secret"), "{seen}");
}

/// M7, the input tree: a symlink in an input directory stays a symlink in the lease's input
/// root (the daemon does not read through it into the file it names or put that file in the
/// cache); a name that is not one path component (`..`, `.`, a slash, empty) is refused
/// with INVALID_ARGUMENT and no file of that name appears anywhere on the farm.
///
/// Catches an input materialiser that dereferences links or lets `..` write outside the
/// input root (daemon.md "Files").
#[test]
fn input_links_and_dotdot_names_do_not_escape() {
    let mut rig = Rig::start("cross-input");
    let secret = canary(4, CANARY_LEN);
    let path = rig.dir.join("secret-file");
    std::fs::write(&path, &secret).expect("secret");
    let link_tree = Tree {
        dirs: vec![(
            "in".to_owned(),
            Tree {
                files: vec![("f".to_owned(), b"x".to_vec())],
                links: vec![("link".to_owned(), text(&path))],
                ..Tree::default()
            },
        )],
        ..Tree::default()
    };
    rig.plant(&[
        "points-to",
        "$ROOT/in/link",
        &text(&path),
        ";",
        "write-lines",
        "$UPPER/out/file.txt",
        "ok",
    ]);
    let kept = rig.run(&Job {
        input: link_tree,
        outputs: vec!["out/file.txt".to_owned()],
        ..Job::default()
    });
    assert_eq!(
        code(&kept),
        0,
        "the link was not a link in the input root: {kept:?}"
    );
    let in_stores = probe::scan(&rig.view().stores(), secret.as_bytes(), &|_| false)
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        in_stores.is_empty(),
        "the secret was cached through a link: {in_stores:?}"
    );
    assert!(
        rig.blob(&digest_of(secret.as_bytes())).is_none(),
        "the secret is in the CAS"
    );

    for name in ["../escape.txt", "..", ".", "a/b", ""] {
        let tree = Tree {
            files: vec![(name.to_owned(), b"zz".to_vec())],
            ..Tree::default()
        };
        let refused = rig.run(&Job {
            input: tree,
            ..Job::default()
        });
        let status = refused.status.as_ref().expect("a status");
        assert_eq!(status.code, 3, "{name:?} was not refused: {refused:?}");
        no_escape(&rig).unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }
}

/// No file named `escape.txt` or `a` anywhere on the farm: a refused name left nothing.
fn no_escape(rig: &Rig) -> Result<(), String> {
    for name in ["escape.txt", "zz"] {
        let found = probe::paths_named(&rig.dir, name)?;
        if !found.is_empty() {
            return Err(format!("{name} exists: {found:?}"));
        }
    }
    Ok(())
}

/// Red for `no_escape` and the link check: an action that makes the file the refused name
/// would have made, and one that reads through the link, are both seen.
#[test]
fn planted_escape_file_and_read_through_link_are_seen() {
    let rig = Rig::start("cross-plant-input");
    rig.plant(&["write-lines", &text(&rig.data().join("escape.txt")), "zz"]);
    let _ = rig.run(&Job::default());
    assert!(no_escape(&rig).is_err());
    let secret = canary(4, CANARY_LEN);
    let path = rig.dir.join("secret-file");
    std::fs::write(&path, &secret).expect("secret");
    let tree = Tree {
        links: vec![("link".to_owned(), text(&path))],
        ..Tree::default()
    };
    rig.plant(&["copy", "$UPPER/out/x", "$ROOT/link"]);
    let read = rig.run(&Job {
        input: tree,
        outputs: vec!["out/x".to_owned()],
        ..Job::default()
    });
    assert!(output_escape(&rig, &read, &secret).is_err());
}

/// M8: the action's descriptors. While A runs, it has descriptor 0 on the null device and
/// 1 and 2 on the lease's `stdout` and `stderr` files, and nothing else: no descriptor of
/// the daemon (another lease's log files, a socket) crossed the exec of the container.
///
/// Catches a daemon descriptor that is not close-on-exec. The probe skips only what the
/// test itself was started with (a build tool's lock file).
#[test]
fn the_action_inherits_only_its_standard_descriptors() {
    let rig = Rig::start("cross-fds");
    let (started, release) = (rig.dir.join("started"), rig.dir.join("release"));
    rig.plant(&[
        "write-lines",
        &text(&started),
        "x",
        ";",
        "wait-for",
        &text(&release),
        "30",
    ]);
    let run = rig.start_run(&Job::default());
    rig.wait_until("the action", || started.exists());
    let pid: u32 = rig.record("pid").trim().parse().expect("the action's pid");
    let fds = probe::open_fds(pid).expect("fds");
    assert!(fds.len() >= 3, "not even the standard descriptors: {fds:?}");
    let verdict = probe::unexpected_fds(&fds);
    std::fs::write(&release, b"").expect("release");
    let _ = rig.join(run);
    verdict.unwrap_or_else(|e| panic!("{e}"));
}

/// Red for the descriptor probe: a process started with the secret file as stdin and one
/// more inherited descriptor (close-on-exec cleared) is reported, both of them.
#[test]
fn planted_inherited_descriptors_are_seen() {
    use std::os::fd::AsFd as _;
    let rig = Rig::start("cross-plant-fds");
    let secret = rig.dir.join("secret-file");
    std::fs::write(&secret, b"secret").expect("secret");
    let extra = std::fs::File::open(&secret).expect("open");
    rustix::io::fcntl_setfd(extra.as_fd(), rustix::io::FdFlags::empty()).expect("fcntl");
    let mut child = std::process::Command::new(rig.bin().join("kbf-act"))
        .args(["sleep", "30"])
        .stdin(std::fs::File::open(&secret).expect("open"))
        .spawn()
        .expect("spawn");
    let _reap = KillOnDrop(vec![child.id()]);
    let verdict = probe::unexpected_fds(&probe::open_fds(child.id()).expect("fds"));
    child.kill().expect("kill");
    child.wait().expect("reap");
    let seen = verdict.expect_err("the extra descriptors were missed");
    assert!(
        seen.contains("fd 0 -> "),
        "stdin is not the null device: {seen}"
    );
    assert!(seen.matches("secret-file").count() >= 2, "{seen}");
}

/// S4: modes of a live lease. While an action runs, nothing in its lease (directory,
/// bundle, input root, upper, work, `rw`) is writable by others, and the input files are
/// read-only. (daemon.md "Files": links to read-only cache entries.)
///
/// Catches a lease directory created world-writable, or a hand-over that opens a
/// directory to every user. On the stand-in the mode table is the real daemon's.
#[test]
fn a_live_lease_is_not_writable_by_others() {
    let rig = Rig::start("cross-modes");
    live_lease(&rig, &[]).unwrap_or_else(|e| panic!("{e}"));
}

/// Runs an action that does `extra` first, parks it, and looks at its lease's modes.
fn live_lease(rig: &Rig, extra: &[&str]) -> Result<(), String> {
    let (started, release) = (rig.dir.join("started"), rig.dir.join("release"));
    let mut steps: Vec<&str> = extra.to_vec();
    if !steps.is_empty() {
        steps.push(";");
    }
    let (started_s, release_s) = (text(&started), text(&release));
    steps.extend([
        "write-lines",
        &started_s,
        "x",
        ";",
        "wait-for",
        &release_s,
        "30",
    ]);
    rig.plant(&steps);
    let run = rig.start_run(&Job {
        input: Tree::with_input("data.bin", b"input"),
        ..Job::default()
    });
    rig.wait_until("the action", || started.exists());
    let root = rig.record("root");
    let lease = std::path::Path::new(root.trim())
        .parent()
        .expect("lease dir")
        .to_owned();
    let open = probe::world_writable(&lease);
    let input = probe::walk(&lease.join("root/in"));
    std::fs::write(&release, b"").expect("release");
    let _ = rig.join(run);
    let open = open?;
    if !open.is_empty() {
        return Err(format!("writable by others: {open:?}"));
    }
    for (rel, entry) in input? {
        if !entry.is_dir() && entry.mode & 0o222 != 0 {
            return Err(format!(
                "input file {} is writable (mode {:o})",
                rel.display(),
                entry.mode
            ));
        }
    }
    Ok(())
}

/// Red for the mode check: the action opens its own lease directory to everyone.
#[test]
fn planted_world_writable_lease_is_seen() {
    let rig = Rig::start("cross-plant-modes");
    let seen = live_lease(&rig, &["chmod", "777", "$ROOT/.."]).expect_err("chmod 777 missed");
    assert!(seen.contains("writable by others"), "{seen}");
}

/// S2, the action cache: a result is served from the cache only for the identical action. B,
/// whose input differs from A's, is run (not served A's result, nor A's stdout); an
/// action identical to A is served from the cache, by design.
///
/// Catches a cache keyed by less than the whole action (the command alone), which would
/// hand B the result, and so the output and stdout, of A.
#[test]
fn a_result_is_cached_only_for_the_identical_action() {
    let rig = Rig::start("cross-cache");
    rig.plant(&["env", "ROOT"]);
    let a = Job {
        input: Tree::with_input("canary.in", canary(5, 4096).as_bytes()),
        cacheable: true,
        ..Job::default()
    };
    let first = rig.run(&a);
    assert!(!first.cached_result);
    let b = Job {
        input: Tree::with_input("canary.in", canary(6, 4096).as_bytes()),
        cacheable: true,
        ..Job::default()
    };
    let other = rig.run(&b);
    not_served_from(&rig, &other, &first).unwrap_or_else(|e| panic!("{e}"));
    let same = rig.run(&a);
    assert!(
        same.cached_result,
        "the identical action was run again: {same:?}"
    );
    // The probe is red for exactly that identical action.
    let seen = not_served_from(&rig, &same, &first).expect_err("a cache hit was missed");
    assert!(seen.contains("served from the cache"), "{seen}");
}

fn not_served_from(rig: &Rig, b: &ExecuteResponse, a: &ExecuteResponse) -> Result<(), String> {
    if b.cached_result {
        return Err("served from the cache".to_owned());
    }
    if rig.stdout_of(b) == rig.stdout_of(a) {
        return Err("B has A's stdout".to_owned());
    }
    Ok(())
}

// ---- tier R: needs real rootless crun; fails loudly when run on the stand-in --------------
//
// kbf-driver-container's tests/oci_crun.rs (`cargo xtask crun-tests`) already shows, for ONE
// lease on real `crun`, that the sandbox is what its config says, that the init's report file
// is out of reach, and that a shared tree cannot be changed (`a_sandbox_cannot_change_a_shared_tree_or_lift_its_read_only_flag`).
// The tests below are the cross-ACTION scenarios (two leases of one node) on top of that.

fn real_crun_needed(what: &str) -> ! {
    panic!(
        "tier R: {what} needs real rootless crun, user and mount namespaces and a delegated \
         cgroup v2 tree; the stand-in has none, and a pass here would prove nothing"
    )
}

/// C1 (X1-X3): B, running while A is parked, reads A's input root, upper, `/tmp` and
/// `rw` by absolute path and by guess, and all fail (A's mount namespace is not B's). On
/// the stand-in B reads them all.
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_a_concurrent_action_cannot_read_a_live_lease() {
    real_crun_needed("B reading A's live lease")
}

/// X1-X3 on uids and modes: a mode-000 file is refused to the action ("permission checks
/// are real"), and a lease directory that is not searchable by the container ids is not
/// entered. (A Mac leg: per-slot users.)
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_modes_are_enforced_across_ids() {
    real_crun_needed("mode and uid enforcement")
}

/// X4/X7: B cannot see A's processes in `/proc` (pid namespace) nor connect to A's
/// loopback or abstract-socket listener (network namespace).
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_pid_and_network_namespaces_hide_a_neighbour() {
    real_crun_needed("pid and network namespaces")
}

/// X6: a write through an input-root file (a link to the shared input cache) copies up
/// into the overlay's upper and never reaches the cache entry or another action's input.
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_writing_an_input_file_does_not_reach_the_cache() {
    real_crun_needed("overlay copy-up of cache links")
}

/// M10: the action's environment is the Command's and nothing of the daemon's (a variable
/// set on the daemon is invisible to the action). The stand-in does not clear it.
#[test]
#[ignore = "needs real rootless crun: cargo xtask crun-tests"]
fn tier_r_the_environment_comes_only_from_the_command() {
    real_crun_needed("a cleared environment")
}
