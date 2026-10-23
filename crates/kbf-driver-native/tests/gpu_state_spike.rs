//! SPIKE (never merges): what a pool user's GPU work leaves behind on a hosted macOS
//! runner, and whether the slot's clean removes it.
//!
//! Two leases in a row on one slot. Each runs, under the shipped slot profile with the
//! lease's `HOME` and `TMPDIR`, the Metal probe (device, runtime shader compile, compute
//! kernel) and the MLX probe (a matmul, `mx.compile`) twice, then starts one holder of
//! each, takes a snapshot while they run, ends the lease's processes as the driver does
//! (SIGTERM, a grace, SIGKILL of the uid's processes), takes a snapshot, removes the
//! lease directory, runs the slot's clean ([`Slot::release`]) and takes a last snapshot.
//! A snapshot is every path of the places below with its owner, size and change time, the
//! I/O Registry's user clients, and the wired memory.
//!
//! Output lines: `TIMING <lease> <arm> ms=N`, `SNAP <label> ...`, `NEW <label> <place>
//! <path> uid=N size=N` (what a snapshot has that the baseline had not), `SEEN <path>`
//! (what the next lease's user can stat), `ARMOUT`. Asserts only that the harness ran.
//! The probes are `tools/spike/macos-apps-gpu`, built into `$KBF_SPIKE_BIN` by the
//! workflow before this test.

#![cfg(target_os = "macos")]
#![allow(clippy::too_many_arguments)]

mod support;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kbf_driver_native::home;
use kbf_driver_native::network::{Isolation, Network};
use kbf_driver_native::slots::macos::{Slot, Slots};
use kbf_driver_native::slots::{LEASES_ROOT_MODE, Pool};
use support::pool::{Scratch, Users};

const ENV_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin";
const GRACE: Duration = Duration::from_secs(3);
/// The most paths one place of a snapshot holds.
const CAP: usize = 600_000;

/// One path of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    uid: u32,
    size: u64,
    dir: bool,
}

/// Paths of one place, by path.
type Tree = BTreeMap<String, Item>;

fn walk(path: &Path, skip: &[&Path], keep: &dyn Fn(&Path, u32) -> bool, out: &mut Tree) {
    if out.len() >= CAP || skip.iter().any(|s| path.starts_with(s)) {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if keep(path, meta.uid()) {
        out.insert(
            path.to_string_lossy().into_owned(),
            Item { uid: meta.uid(), size: meta.size(), dir: meta.is_dir() },
        );
    }
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for e in entries.flatten() {
            walk(&e.path(), skip, keep, out);
        }
    }
}

fn tree(root: &Path, skip: &[&Path], keep: &dyn Fn(&Path, u32) -> bool) -> Tree {
    let mut out = Tree::new();
    walk(root, skip, keep, &mut out);
    out
}

/// What the I/O Registry says about user clients.
#[derive(Default)]
struct Registry {
    total: usize,
    of_uid: Vec<String>,
    dead: Vec<String>,
}

fn text(program: &str, args: &[&str]) -> String {
    let out = Command::new(program).args(args).stdin(Stdio::null()).output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// pid to uid of every process now.
fn processes() -> BTreeMap<i32, u32> {
    text("/bin/ps", &["-ax", "-o", "pid=,uid="])
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect()
}

fn registry(uid: u32) -> Registry {
    let procs = processes();
    let mut r = Registry::default();
    for line in text("/usr/sbin/ioreg", &["-l", "-w0"]).lines() {
        let Some(at) = line.find("\"IOUserClientCreator\" = \"pid ") else {
            continue;
        };
        r.total += 1;
        let rest = &line[at + 29..];
        let pid: i32 = rest.split(|c: char| !c.is_ascii_digit()).next().and_then(|p| p.parse().ok()).unwrap_or(-1);
        let said: String = line.trim().chars().take(200).collect();
        match procs.get(&pid) {
            Some(u) if *u == uid => r.of_uid.push(said),
            None => r.dead.push(said),
            _ => {}
        }
    }
    r
}

/// Wired memory in MiB, from `vm_stat`.
fn wired_mib() -> Option<u64> {
    let out = text("/usr/bin/vm_stat", &[]);
    let size: u64 = out.lines().next()?.split("page size of ").nth(1)?.split_whitespace().next()?.parse().ok()?;
    let pages: u64 = out
        .lines()
        .find(|l| l.starts_with("Pages wired down"))?
        .split(':')
        .nth(1)?
        .trim()
        .trim_end_matches('.')
        .parse()
        .ok()?;
    Some(pages * size / 1024 / 1024)
}

/// One snapshot.
struct Snap {
    places: Vec<(&'static str, Tree)>,
    reg: Registry,
    wired: Option<u64>,
}

struct Where {
    home: PathBuf,
    folders: PathBuf,
    uid: u32,
    logs: PathBuf,
}

fn snapshot(w: &Where, label: &str) -> Snap {
    let started = Instant::now();
    let uid = w.uid;
    let all = |_: &Path, _: u32| true;
    let mut places: Vec<(&'static str, Tree)> = Vec::new();
    places.push(("home", tree(&w.home, &[], &all)));
    places.push(("var-folders-entry", tree(&w.folders, &[], &all)));
    // Every metal cache folder of every /var/folders entry, whoever owns it.
    let mut metal = Tree::new();
    for a in std::fs::read_dir("/private/var/folders").into_iter().flatten().flatten() {
        for b in std::fs::read_dir(a.path()).into_iter().flatten().flatten() {
            for c in std::fs::read_dir(b.path().join("C")).into_iter().flatten().flatten() {
                if c.file_name().to_string_lossy().starts_with("com.apple.metal") {
                    walk(&c.path(), &[], &all, &mut metal);
                }
            }
        }
    }
    places.push(("var-folders-com.apple.metal*", metal));
    // Everything of the uid in /var/folders, wherever it is.
    places.push((
        "var-folders-owned-by-uid",
        tree(Path::new("/private/var/folders"), &[], &|_, u| u == uid),
    ));
    let db = |p: &Path, u: u32| p.starts_with("/private/var/db") || u == uid;
    let skip: [&Path; 3] = [
        Path::new("/private/var/vm"),
        Path::new("/private/var/folders"),
        &w.home,
    ];
    let mut rest = Tree::new();
    for root in ["/private/var", "/Library", "/Users", "/private/tmp", "/cores", "/Applications"] {
        walk(Path::new(root), &skip, &|p, u| db(p, u) || p.starts_with("/Library/Caches"), &mut rest);
    }
    let split = |keep: &dyn Fn(&str) -> bool| -> Tree {
        rest.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    places.push(("var-db", split(&|k| k.starts_with("/private/var/db"))));
    places.push(("Library-Caches", split(&|k| k.starts_with("/Library/Caches"))));
    places.push((
        "owned-by-uid-elsewhere",
        rest.iter()
            .filter(|(k, v)| v.uid == uid && !k.starts_with("/private/var/db") && !k.starts_with("/Library/Caches"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    ));
    let reg = registry(uid);
    let wired = wired_mib();
    let snap = Snap { places, reg, wired };

    let mut file = String::new();
    for (place, t) in &snap.places {
        for (path, item) in t {
            let _ = writeln!(file, "{place}\t{path}\tuid={}\tsize={}", item.uid, item.size);
        }
    }
    let _ = std::fs::write(w.logs.join(format!("{label}.snap")), file);
    let mut reg_text = String::new();
    for l in &snap.reg.of_uid {
        let _ = writeln!(reg_text, "OF_UID {l}");
    }
    for l in &snap.reg.dead {
        let _ = writeln!(reg_text, "DEAD {l}");
    }
    let _ = std::fs::write(w.logs.join(format!("{label}.ioreg")), reg_text);
    let counts: Vec<String> = snap.places.iter().map(|(p, t)| format!("{p}={}", t.len())).collect();
    println!(
        "SNAP {label} secs={} wired_mib={:?} ioreg_user_clients={} of_uid={} dead_creators={} {}",
        started.elapsed().as_secs(),
        snap.wired,
        snap.reg.total,
        snap.reg.of_uid.len(),
        snap.reg.dead.len(),
        counts.join(" ")
    );
    for l in snap.reg.of_uid.iter().take(8) {
        println!("IOREG {label} of_uid | {l}");
    }
    for l in snap.reg.dead.iter().take(8) {
        println!("IOREG {label} dead | {l}");
    }
    println!(
        "GPUMEM {label} | {}",
        text("/usr/sbin/ioreg", &["-r", "-c", "IOAccelerator", "-w0"])
            .lines()
            .filter(|l| l.contains("PerformanceStatistics"))
            .map(|l| l.trim().chars().take(900).collect::<String>())
            .collect::<Vec<_>>()
            .join(" || ")
    );
    println!(
        "MEMPRESSURE {label} | {}",
        text("/usr/bin/memory_pressure", &[]).lines().last().unwrap_or("").trim()
    );
    snap
}

/// What `now` has that `base` had not (a path, or a changed size of a file), and which of
/// those lie below `ignore`.
fn new_in<'a>(base: &Snap, now: &'a Snap, ignore: &Path) -> Vec<(&'static str, &'a String, &'a Item)> {
    let mut out = Vec::new();
    for (place, t) in &now.places {
        let was = base.places.iter().find(|(p, _)| p == place).map(|(_, t)| t);
        for (path, item) in t {
            if Path::new(path).starts_with(ignore) {
                continue;
            }
            let old = was.and_then(|w| w.get(path));
            if old.is_none_or(|o| !item.dir && o.size != item.size) {
                out.push((*place, path, item));
            }
        }
    }
    out
}

fn report_new(label: &str, base: &Snap, now: &Snap, ignore: &Path) -> Vec<String> {
    let found = new_in(base, now, ignore);
    let mut by_place: BTreeMap<&str, usize> = BTreeMap::new();
    for (place, path, item) in &found {
        let n = by_place.entry(place).or_default();
        *n += 1;
        if *n <= 80 {
            println!("NEW {label} {place} {path} uid={} size={}", item.uid, item.size);
        }
    }
    println!("NEWCOUNT {label} {by_place:?}");
    found.iter().map(|(_, p, _)| (*p).clone()).collect()
}

struct Env {
    bin: String,
    logs: PathBuf,
    isolation: Isolation,
}

/// The arm `program args` as the lease's user under the slot profile, with the lease's
/// variables, stopped by an alarm after `secs`.
fn launch(slot: &Slot, env: &Env, lease: &Path, vars: &[(&str, std::ffi::OsString)], extra: &[String], program: &str, args: &[&str], secs: u64) -> Command {
    let mut inner: Vec<String> = vec!["-e".into(), "alarm shift; exec @ARGV".into(), secs.to_string(), "/usr/bin/env".into()];
    for (k, v) in vars {
        inner.push(format!("{k}={}", v.to_string_lossy()));
    }
    inner.push(format!("PATH={ENV_PATH}"));
    inner.extend(extra.iter().cloned());
    inner.push(program.to_owned());
    inner.extend(args.iter().map(|a| (*a).to_owned()));
    let (prog, argv) = slot
        .launch(&env.isolation, Network::Off, lease, PathBuf::from("/usr/bin/perl"), &inner)
        .expect("launch");
    let mut c = Command::new(prog);
    c.args(argv).env_clear().current_dir("/").stdin(Stdio::null());
    c
}

fn run_arm(slot: &Slot, env: &Env, lease: &Path, vars: &[(&str, std::ffi::OsString)], tag: &str, program: &str, args: &[&str]) {
    let file = env.logs.join(format!("{tag}.out"));
    let f = File::create(&file).expect("out");
    let started = Instant::now();
    let status = launch(slot, env, lease, vars, &[], program, args, 300)
        .stdout(f.try_clone().expect("clone"))
        .stderr(f)
        .status();
    println!("TIMING {tag} ms={} exit={:?}", started.elapsed().as_millis(), status.map(|s| s.code()));
    let out = std::fs::read_to_string(&file).unwrap_or_default();
    for l in out.lines().filter(|l| l.contains("STAGE") || l.contains("MLX") || l.contains("DEVICES") || l.contains("PASS") || l.contains("FAIL")).take(40) {
        println!("ARMOUT {tag} | {}", l.chars().take(300).collect::<String>());
    }
}

fn start_holder(slot: &Slot, env: &Env, lease: &Path, vars: &[(&str, std::ffi::OsString)], tag: &str, program: &str, args: &[&str]) -> (Child, PathBuf) {
    let file = env.logs.join(format!("{tag}.out"));
    let f = File::create(&file).expect("out");
    let child = launch(slot, env, lease, vars, &["GPU_HOLD=1".to_owned()], program, args, 900)
        .stdout(f.try_clone().expect("clone"))
        .stderr(f)
        .spawn()
        .expect("spawn");
    (child, file)
}

fn wait_for(file: &Path, needle: &str, secs: u64) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if std::fs::read_to_string(file).is_ok_and(|t| t.contains(needle)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

fn uid_pids(uid: u32) -> usize {
    processes().values().filter(|u| **u == uid).count()
}

/// The end of a lease as the driver makes it: SIGTERM to every process of the uid, the
/// grace, SIGKILL to what is left.
fn end_processes(uid: u32, label: &str) {
    let u = uid.to_string();
    let _ = Command::new("/usr/bin/pkill").args(["-TERM", "-U", &u]).status();
    std::thread::sleep(GRACE);
    println!("KILL {label} after_term_and_grace processes_of_uid={}", uid_pids(uid));
    let _ = Command::new("/usr/bin/pkill").args(["-KILL", "-U", &u]).status();
    std::thread::sleep(Duration::from_secs(2));
    println!("KILL {label} after_kill processes_of_uid={}", uid_pids(uid));
}

async fn lease(slots: &Slots, scratch: &Scratch, n: u32) -> (Slot, PathBuf, Vec<(&'static str, std::ffi::OsString)>) {
    let leases = scratch.leases();
    std::fs::create_dir_all(&leases).expect("leases");
    std::fs::set_permissions(&leases, std::fs::Permissions::from_mode(LEASES_ROOT_MODE)).expect("chmod");
    let dir = leases.join(format!("lease-{n}-1"));
    std::fs::create_dir(&dir).expect("lease dir");
    let vars = home::make(&dir).await.expect("home vars");
    let slot = slots.take().await.expect("a slot");
    slot.hand_over(&dir).await.expect("hand over");
    (slot, std::fs::canonicalize(&dir).expect("real"), vars)
}

#[ignore = "needs root, real users and the probes: see spike-slot-reach.yml job apps-gpu"]
#[tokio::test(flavor = "multi_thread")]
async fn gpu_state_after_the_kill_and_after_the_clean_is_recorded() {
    support::pool::assert_root("gpu_state_spike");
    let scratch = Scratch::new("gpustate");
    let users = Users::new("gpustate", 1);
    let pool = Pool::new(users.0.clone()).expect("pool");
    let logs = PathBuf::from(std::env::var("KBF_SPIKE_LOGS").unwrap_or_else(|_| "/private/tmp/kbf-spike-logs".into()));
    std::fs::create_dir_all(&logs).expect("logs");
    let env = Env {
        bin: std::env::var("KBF_SPIKE_BIN").unwrap_or_else(|_| "/private/tmp/kbf-spike-bin".into()),
        logs: logs.clone(),
        isolation: Isolation::detect(),
    };
    println!("SW_VERS {}", text("/usr/bin/sw_vers", &[]).replace('\n', " "));
    println!("HW {}", text("/usr/sbin/sysctl", &["-n", "hw.model", "hw.memsize", "hw.ncpu"]).replace('\n', " "));
    let slots = Slots::start_with_grace(&pool, Duration::from_secs(20), GRACE, None).expect("slots");
    let user = users.0[0].clone();

    // The first lease only to learn the slot's folders; the baseline is taken before any GPU use.
    let probe = lease(&slots, &scratch, 0).await;
    let ready = probe.0.ready();
    let w = Where { home: ready.home.clone(), folders: ready.folders.clone(), uid: user.uid, logs: logs.clone() };
    println!("SETUP user={} uid={} home={} folders={}", user.name, user.uid, w.home.display(), w.folders.display());
    probe.0.release().await;
    let _ = kbf_outputs::remove_tree(&probe.1);
    let baseline = snapshot(&w, "00-baseline");

    let metal = format!("{}/gpu_probe", env.bin);
    let mlx_py = format!("{}/venv/bin/python", env.bin);
    let mlx_script = format!("{}/mlx_probe.py", env.bin);
    let mut survivors: Vec<String> = Vec::new();
    for round in 1..=2u32 {
        let tag = format!("L{round}");
        let (slot, dir, vars) = lease(&slots, &scratch, round).await;
        if round == 2 && !survivors.is_empty() {
            // What the next lease on the same uid can reach of what survived the clean.
            let list: Vec<&str> = survivors.iter().take(300).map(String::as_str).collect();
            let out = launch(&slot, &env, &dir, &vars, &[], "/bin/ls", &[&["-ld"][..], &list[..]].concat(), 60)
                .stderr(Stdio::null())
                .output()
                .expect("ls");
            let seen = String::from_utf8_lossy(&out.stdout);
            println!("SEEN-COUNT {tag} survivors={} stat_visible={}", list.len(), seen.lines().count());
            for l in seen.lines().take(60) {
                println!("SEEN {tag} | {l}");
            }
        }
        for arm in ["a", "b"] {
            run_arm(&slot, &env, &dir, &vars, &format!("{tag}-metal-{arm}"), &metal, &[]);
        }
        for arm in ["a", "b"] {
            run_arm(&slot, &env, &dir, &vars, &format!("{tag}-mlx-{arm}"), &mlx_py, &[&mlx_script]);
        }
        let after_arms = snapshot(&w, &format!("{round}1-{tag}-after-arms-processes-ended"));
        report_new(&format!("{tag}-after-arms"), &baseline, &after_arms, &dir);

        let (mut h1, f1) = start_holder(&slot, &env, &dir, &vars, &format!("{tag}-hold-metal"), &metal, &[]);
        let (mut h2, f2) = start_holder(&slot, &env, &dir, &vars, &format!("{tag}-hold-mlx"), &mlx_py, &[&mlx_script]);
        println!("HOLDERS {tag} metal_ready={} mlx_ready={}", wait_for(&f1, "HOLD_READY", 180), wait_for(&f2, "HOLD_READY", 240));
        let during = snapshot(&w, &format!("{round}2-{tag}-during"));
        report_new(&format!("{tag}-during"), &baseline, &during, &dir);

        end_processes(user.uid, &tag);
        let _ = h1.wait();
        let _ = h2.wait();
        let killed = snapshot(&w, &format!("{round}3-{tag}-after-kill"));
        report_new(&format!("{tag}-after-kill"), &baseline, &killed, &dir);

        // The lease directory goes first, as the daemon does, then the clean.
        slot.reclaim(&dir).await.expect("reclaim");
        let _ = kbf_outputs::remove_tree(&dir);
        let t = Instant::now();
        slot.release().await;
        println!("CLEAN {tag} ms={} free_slots={}", t.elapsed().as_millis(), slots.count());
        let cleaned = snapshot(&w, &format!("{round}4-{tag}-after-clean"));
        survivors = report_new(&format!("{tag}-after-clean"), &baseline, &cleaned, &dir);
        println!("SURVIVORS {tag} {}", survivors.len());
    }
    drop(slots);
    drop(users);
    let last = snapshot(&Where { uid: user.uid, ..w }, "99-after-users-deleted");
    report_new("after-users-deleted", &baseline, &last, Path::new("/nonexistent"));
    println!("LEASES_ROOT_MODE {LEASES_ROOT_MODE:o} DONE");
}
