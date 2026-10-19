//! The OCI backend against real rootless `crun`, with the same setup as `crun.rs`
//! (`cargo xtask crun-tests`: a `Delegate=yes` unit, `crun` and `uidmap` on the runner;
//! the busybox image is fetched by the driver's own image store, digest-verified).
//!
//! Every test is `#[ignore]` with that reason and the verb runs them with
//! `--include-ignored`; run without the setup a test fails, it never skips silently.
//! What the stand-in `crun` cannot say, these do: the bundle is accepted by `crun`, the
//! sandbox is what `config.json` says it is when read from inside, the exit code is
//! `crun run`'s, and the image unpacked with the ids the action sees.

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use kbf_daemon::RuntimeError;
use support::real::{Cell, sh, var};
use support::{Spec, blob};

/// The kernel's last words about overlayfs and user namespaces (`dmesg`, through `sudo -n`:
/// a hosted runner has it), printed when a lease fails: the refusal of a mount is in the
/// kernel log, not in `crun`'s words.
fn kernel_log() -> String {
    let out = Command::new("sudo").args(["-n", "dmesg"]).output();
    let text = out.map_or_else(
        |e| e.to_string(),
        |o| String::from_utf8_lossy(&o.stdout).into_owned(),
    );
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines.len().saturating_sub(25);
    format!("kernel log: {:#?}", &lines[tail..])
}

/// `cell.run`, with the kernel log printed if the lease failed.
async fn run(
    cell: &Cell,
    seq: u64,
    spec: &Spec,
) -> Result<kbf_proto::reapi::ActionResult, kbf_daemon::RuntimeError> {
    let result = cell.run(seq, spec).await;
    if result.is_err() {
        eprintln!("{}", kernel_log());
    }
    result
}

/// Catches the sandbox being less than `config.json` says, read from inside: the
/// action is the unprivileged uid 1000 (no capability held, the bounding set kept) with every
/// id a subordinate id, alone in its pid namespace, with the
/// constant hostname and loopback only, behind a seccomp filter with the eleven
/// capabilities, and with no id of the daemon user (a file the daemon owns is
/// `nobody`'s). The first lease unpacks the image, so this also runs the whole
/// fetch-and-unpack path.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn the_sandbox_is_what_the_config_says() {
    let cell = Cell::new("sandbox");
    let script = r#"
        echo "uid $(id -u):$(id -g) pid $$ host $(hostname)"
        echo "map $(cat /proc/self/uid_map)"
        echo "net $(ls /sys/class/net | tr '\n' ' ')"
        grep -E '^(Seccomp|NoNewPrivs|CapEff|CapPrm|CapBnd|CapAmb):' /proc/self/status
        echo "fds $(ls /proc/self/fd | tr '\n' ' ')"
        echo "kcore $(wc -c < /proc/kcore)"
        (echo x > /proc/sys/kernel/hostname) 2>/dev/null && echo "proc-sys writable" || echo "proc-sys read-only"
        (touch /new-file) 2>/dev/null && echo "root writable" || echo "root read-only"
        (echo x > /tmp/f && echo x > /root/f && echo x > /var/tmp/f) && echo "tmp writable"
        echo "owner $(stat -c %u /kbf/root/in.txt)"
        (mount -o remount,rw /) 2>/dev/null && echo "remount ok" || echo "remount refused"
    "#;
    let mut spec = sh(script);
    spec.inputs = vec![("in.txt", b"in", false)];
    let result = run(&cell, 1, &spec).await.expect("ran");
    let out = cell.stdout(&result);
    assert_eq!(result.exit_code, 0, "{out}\n{}", cell.stderr(&result));
    let subuid = first_subordinate_id();
    // Pid 1 is the container's init (`init/kbf_init.rs`), and the action is its child.
    assert!(out.contains("uid 1000:1000 pid 2 host localhost"), "{out}");
    assert!(
        out.contains(&format!("map          0 {subuid:>10}      65536")),
        "{out}"
    );
    assert!(out.contains("net lo"), "{out}");
    assert!(out.contains("Seccomp:\t2"), "{out}");
    // A non-root process holds no capability after `exec`, whatever the bounding set.
    assert!(out.contains("CapEff:\t0000000000000000"), "{out}");
    assert!(out.contains("CapPrm:\t0000000000000000"), "{out}");
    assert!(out.contains("CapBnd:\t00000000800405fb"), "{out}");
    assert!(out.contains("CapAmb:\t0000000000000000"), "{out}");
    // 0, 1, 2 and the descriptor `ls` itself holds: nothing of the host leaks in.
    assert!(out.contains("fds 0 1 2 3"), "{out}");
    assert!(out.contains("kcore 0"), "{out}");
    assert!(out.contains("proc-sys read-only"), "{out}");
    assert!(out.contains("root read-only"), "{out}");
    assert!(out.contains("tmp writable"), "{out}");
    assert!(out.contains("remount refused"), "{out}");
    cell.assert_clean(1);
}

/// The first subordinate uid of the test's user: what container id 0 is on the host.
fn first_subordinate_id() -> u32 {
    let uid = rustix::process::getuid().as_raw().to_string();
    let user = std::env::var("USER").unwrap_or_default();
    let text = std::fs::read_to_string("/etc/subuid").expect("/etc/subuid");
    text.lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let owner = fields.next()?;
            let first = fields.next()?.parse().ok()?;
            (owner == uid || owner == user).then_some(first)
        })
        .expect("a subordinate range for the test user")
}

/// Catches the exit code not being `crun run`'s own (the "stopped 0" window of kbf#658
/// is closed by design, and this is the check that it stays closed): fifty short
/// actions in a row, each with its own code, and a signal death as the shell reports it.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn every_exit_code_is_the_actions_own() {
    let cell = Cell::new("exit-codes");
    let started = Instant::now();
    for seq in 1..=50_u64 {
        let code = i32::try_from(seq * 5 % 256).expect("small");
        let result = cell
            .run(seq, &sh(&format!("exit {code}")))
            .await
            .expect("ran");
        assert_eq!(result.exit_code, code, "lease {seq}");
        cell.assert_clean(seq);
    }
    eprintln!("50 leases in {:?}", started.elapsed());
    // A death by signal is the shell's `128 + n`, whether the action dies of it itself
    // (it is the init's child, not pid 1, so the signal is delivered) or a child does.
    for (seq, (script, code)) in (51..).zip([
        ("kill -KILL $$", 137),
        ("kill -TERM $$", 143),
        ("sh -c 'kill -KILL $$'; exit $(($? - 128))", 9),
    ]) {
        let result = cell.run(seq, &sh(script)).await.expect("ran");
        assert_eq!(result.exit_code, code, "{script}");
        cell.assert_clean(seq);
    }
}

/// Catches an orphaned process staying a zombie, which the mopm pilot found
/// (`src/core/test_proc.zig`: a test kills its process group and `kill(-pgid, 0)` still
/// finds it, because the orphan was adopted by a container's pid 1 that never waits).
/// The action starts a session leader in the background from a subshell that exits
/// at once, so the leader is an orphan; the action kills it, and 2 s later it must not
/// be in `/proc` at all (a zombie is, and so is a group that `kill(-pgid, 0)` finds).
/// Pid 1 must be the init and the leader its child. The mutant: an init that waits only
/// for the action leaves `Z` and exit 3.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn an_orphan_is_reaped_so_a_killed_group_is_gone() {
    let cell = Cell::new("reaper");
    let script = r#"
        ( /bin/setsid /bin/sh -c 'echo $$ > /tmp/leader; exec /bin/sleep 300' & )
        n=0
        while [ ! -s /tmp/leader ] && [ $n -lt 50 ]; do /bin/sleep 1; n=$((n + 1)); done
        leader=$(cat /tmp/leader)
        echo "pid1 $(tr '\0' ' ' < /proc/1/cmdline)"
        echo "parent $(sed -n 's/^PPid:[[:space:]]*//p' /proc/$leader/status)"
        kill -9 $leader
        /bin/sleep 2
        if [ -e /proc/$leader ]; then echo "leftover $(cat /proc/$leader/stat)"; exit 3; fi
        echo reaped
    "#;
    let result = cell.run(1, &sh(script)).await.expect("ran");
    let out = cell.stdout(&result);
    assert_eq!(result.exit_code, 0, "{out}\n{}", cell.stderr(&result));
    assert!(out.contains("pid1 /dev/kbf-init -- /bin/sh -c "), "{out}");
    assert!(out.contains("\nparent 1\n"), "{out}");
    assert!(out.ends_with("reaped\n"), "{out}");
    cell.assert_clean(1);
}

/// Catches the init outliving or outwaiting the action: a lease ends with the action's
/// status when the action exits while a background process of it still runs (the
/// container's other processes die with its first, as they did when that was the
/// action), within seconds and not the background process's 300.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn the_lease_ends_with_the_action_whatever_it_left_running() {
    let cell = Cell::new("reaper-ends");
    let started = Instant::now();
    let result = cell
        .run(1, &sh("sleep 300 & sleep 300 & exit 4"))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 4, "{}", cell.stderr(&result));
    assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
    cell.assert_clean(1);
}

/// Catches a program crun cannot start being the farm's failure, not the action's:
/// the same cases a stand-in `crun` is run for, through `crun`'s own log (a bare name with no
/// `PATH`, one missing on the `PATH`, a missing path: exit 127; a file that is not
/// executable and a directory: 126), each with kbf's message.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn a_program_crun_cannot_start_is_the_actions_failure() {
    let cell = Cell::new("program");
    let action = |argv: &[&str], env: &[(&str, &str)]| {
        let mut spec = sh("unused");
        spec.argv = argv.iter().map(|a| (*a).to_owned()).collect();
        spec.env = env
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect();
        spec.inputs = vec![("data", b"not a program", false)];
        spec
    };
    let cases = [
        (action(&["env"], &[]), 127, "the Command sets no PATH"),
        (
            action(&["no-such-tool"], &[("PATH", "/usr/bin:/bin")]),
            127,
            "PATH \"/usr/bin:/bin\"",
        ),
        (action(&["/no/such/tool"], &[]), 127, "was not found"),
        (action(&["./data"], &[]), 126, "not an executable file"),
        (action(&["/bin"], &[]), 126, "not an executable file"),
    ];
    for (seq, (spec, code, says)) in (1..).zip(cases) {
        let result = cell.run(seq, &spec).await;
        let result = result.unwrap_or_else(|e| panic!("{:?}: {e:?}", spec.argv));
        let stderr = cell.stderr(&result);
        assert_eq!(result.exit_code, code, "{:?}: {stderr}", spec.argv);
        assert!(stderr.starts_with("kbf: "), "{stderr}");
        assert!(stderr.contains(says), "{stderr}");
        cell.assert_clean(seq);
    }
}

/// Catches inputs and outputs going wrong through the overlay and the hand-over: an
/// input the action reads and an output it writes are collected whole, a file it makes
/// private (mode 0600, in a directory 0700) is still read by the daemon's user after the
/// hand-back, and nothing it writes to `/tmp` or elsewhere in the container's writable
/// places comes back as an output.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn outputs_come_back_through_the_overlay_and_the_hand_over() {
    let cell = Cell::new("outputs");
    let mut spec = sh(
        "cat in.txt > out/copy.txt; umask 077; mkdir out/priv; echo secret > out/priv/f; \
         echo scratch > /tmp/not-an-output",
    );
    spec.inputs = vec![("in.txt", b"input bytes", false)];
    spec.outputs = vec!["out/copy.txt".to_owned(), "out/priv".to_owned()];
    let result = run(&cell, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", cell.stderr(&result));
    assert_eq!(result.output_files.len(), 1, "{result:?}");
    assert_eq!(
        blob(&cell.cas, result.output_files[0].digest.as_ref()),
        b"input bytes"
    );
    assert_eq!(result.output_directories.len(), 1, "{result:?}");
    let tree = support::tree(&cell.cas, result.output_directories[0].tree_digest.as_ref());
    let root = tree.root.expect("root");
    assert_eq!(root.files.len(), 1, "{root:?}");
    assert_eq!(blob(&cell.cas, root.files[0].digest.as_ref()), b"secret\n");
    cell.assert_clean(1);
}

/// Catches a timeout or a kill leaving the container, its processes or its cgroup
/// behind on real `crun`: a timed-out action keeps its partial stdout, and a killed
/// lease is clean when `kill` returns.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn timeout_and_kill_leave_nothing_behind() {
    use kbf_daemon::Runtime;
    let cell = Cell::new("timeout-kill");
    let mut spec = sh("echo started; sleep 300");
    spec.timeout = Some(std::time::Duration::from_secs(2));
    let outcome = run(&cell, 1, &spec).await;
    let Err(RuntimeError::TimedOut { stdout, .. }) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(blob(&cell.cas, stdout.as_ref()), b"started\n");
    cell.assert_clean(1);

    let action = store_for(&cell, &sh("sleep 300"));
    let runtime = std::sync::Arc::clone(&cell.runtime);
    let work = cell.work(2, action, kbf_types::Resources::new(1000, 256 << 20));
    let run = tokio::spawn(async move { runtime.run(work).await });
    for _ in 0..200 {
        if cell.cgroup.join(cell.name(2)).join("ctr").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    cell.runtime
        .kill(kbf_types::LeaseId::new(cell.term, 2))
        .await;
    assert!(matches!(
        run.await.expect("join"),
        Err(RuntimeError::Killed)
    ));
    cell.assert_clean(2);
}

fn store_for(cell: &Cell, spec: &Spec) -> kbf_proto::reapi::Digest {
    support::store_action(&cell.cas, spec)
}

/// Catches the container's limits missing on real `crun`: `pids.max` on the container's
/// cgroup (a child of the lease's) is the configured limit, and the lease cgroup keeps
/// its own memory limit and `memory.oom.group`.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn the_container_cgroup_carries_the_pids_limit_and_the_oom_group() {
    let cell = Cell::with("limits", |c| c.limits.pids = 321);
    let lease = cell.cgroup.join(cell.name(1));
    let script = format!(
        "cat /sys/fs/cgroup/pids.max; cat /sys/fs/cgroup/memory.oom.group; \
         (echo 1 > /sys/fs/cgroup/pids.max) 2>/dev/null && echo raised || echo not-raised; \
         sleep 1; echo {}",
        lease.display()
    );
    let result = cell.run(1, &sh(&script)).await.expect("ran");
    let out = cell.stdout(&result);
    assert_eq!(result.exit_code, 0, "{out}\n{}", cell.stderr(&result));
    assert!(out.starts_with("321\n1\nnot-raised\n"), "{out}");
}

/// Catches an image not unpacked with the ids the action sees: after the first lease,
/// the unpacked root is owned by the first subordinate id and `/bin` inside is root's.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn the_unpacked_image_is_owned_by_the_containers_ids() {
    use std::os::unix::fs::MetadataExt as _;
    let cell = Cell::new("image-owner");
    let result = cell
        .run(
            1,
            &sh("stat -c '%u:%g' /bin /etc/passwd /; ls -ln / | head -3"),
        )
        .await
        .expect("ran");
    let out = cell.stdout(&result);
    assert_eq!(result.exit_code, 0, "{out}\n{}", cell.stderr(&result));
    assert!(out.starts_with("0:0\n0:0\n0:0\n"), "{out}");
    let hex = var("KBF_TEST_IMAGE");
    let hex = hex.rsplit("sha256:").next().expect("digest").to_owned();
    let rootfs = cell.scratch.join("oci-images").join(&hex).join("rootfs");
    let meta = std::fs::metadata(&rootfs).expect("the unpacked image");
    assert_eq!(meta.uid(), first_subordinate_id(), "{}", rootfs.display());
    // A second lease finds it unpacked: it costs the run alone.
    let started = Instant::now();
    cell.run(2, &sh("true")).await.expect("ran again");
    eprintln!("a lease on an unpacked image: {:?}", started.elapsed());
}

/// A cache and a tree store beside `config`'s scratch directory: leases bind the large
/// Directory nodes of an input root from the tree store, not link them.
fn with_trees(config: &mut kbf_driver_container::ContainerConfig) {
    let dir = PathBuf::from(format!("{}-cache", config.scratch.display()));
    support::force_remove(&dir);
    std::fs::create_dir_all(&dir).expect("cache dir");
    let cache = kbf_daemon::cache::CacheConfig::new(dir, config.scratch.clone(), 1 << 30);
    let trees = PathBuf::from(format!("{}-trees", config.scratch.display()));
    support::force_remove(&trees);
    let mut trees = kbf_daemon::trees::TreeConfig::new(trees, 1 << 30);
    trees.min_files = 20;
    config.cache = Some(kbf_driver_container::CacheUse {
        config: cache,
        helper: config.oci.daemon.clone(),
        trees: Some(trees),
    });
}

/// Catches a shared tree the sandbox can write or lift the read-only flag from, which
/// the config-level unit test cannot: the action tries to create, append, truncate,
/// chmod, mkdir, rename, hard-link out of and delete inside the tree, and to remount it
/// read-write as the container's root. Every attempt is refused, the first with
/// `EROFS`'s own words, and the next lease reads the tree whole.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn a_sandbox_cannot_change_a_shared_tree_or_lift_its_read_only_flag() {
    let cell = Cell::with("trees-ro", with_trees);
    let mut spec = sh("grep ' /kbf/root/tc ' /proc/self/mountinfo; \
         echo \"first: $(sh -c 'echo x > tc/new' 2>&1)\"; \
         for try in 'echo x > tc/new' 'echo x >> tc/lib/f1' 'truncate -s0 tc/lib/f1' 'chmod u+w tc/lib/f1' \
                    'mkdir tc/d' 'mv tc/lib/f1 tc/lib/g' 'mv tc/lib/f2 f2' 'ln tc/lib/f3 f3' \
                    'rm tc/lib/f4' 'mount -o remount,rw /kbf/root/tc'; do \
           if sh -c \"$try\" 2>/dev/null; then echo \"WROTE: $try\"; else echo \"refused: $try\"; fi; \
         done; cat tc/lib/f1; echo; ls tc/lib | wc -l");
    spec.inputs = (0..30)
        .map(|i| {
            let path: &'static str = Box::leak(format!("tc/lib/f{i}").into_boxed_str());
            let bytes: &'static [u8] =
                Box::leak(format!("tool file {i}").into_boxed_str()).as_bytes();
            (path, bytes, i == 0)
        })
        .collect();
    spec.inputs.push(("src/a", b"small", false));
    let result = run(&cell, 1, &spec).await.expect("ran");
    let said = cell.stdout(&result);
    eprintln!("{said}");
    assert!(!said.contains("WROTE"), "{said}");
    assert_eq!(said.matches("refused: ").count(), 10, "{said}");
    assert!(said.contains("Read-only file system"), "{said}");
    assert!(
        said.lines().next().is_some_and(|l| l.contains(" ro")),
        "{said}"
    );
    assert!(said.ends_with("tool file 1\n30\n"), "{said}");
    let stats = cell.runtime.tree_stats().expect("a store");
    assert_eq!(stats.builds, 1, "{stats:?}");
    cell.assert_clean(1);
}
