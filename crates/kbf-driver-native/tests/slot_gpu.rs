//! What a pool lease's GPU work leaves behind, and that the slot's clean removes it, on a
//! Mac with a Metal device (docs/design/macos-end-state.md, "GPU state of a pool lease").
//!
//! One lease on a pool of one user runs, under the slot profile with the lease's `HOME`
//! and `TMPDIR`, a Metal probe (`tests/slot_gpu/gpu_probe.swift`: a device, a shader
//! compiled at run time, a compute kernel), then a second copy that keeps its device
//! open, so that the I/O Registry has a user client of the uid. The lease ends as the
//! driver ends it, the lease directory goes, and the clean runs. The files that a
//! framework import (MLX) makes in the security-message folder, which the Metal probe
//! does not, are planted the way the system makes them: owned by the uid.
//!
//! The analytics and trust stores under `/private/var/protected` are not swept in the
//! daemon's context on a real Mac (root is refused there, even with Full Disk Access for
//! the trust store); the slot profile denies leases access to them instead. The test
//! checks that the deny holds for a lease (a readable file of the uid's in each store is
//! unreadable under the profile), that the Metal probe still runs under it, and that an
//! MLX matrix product still runs under it when MLX can be installed on the runner.
//!
//! The test is `#[ignore]`: it needs root, real users, a Metal device and `swiftc`. The
//! macOS CI job runs it with `cargo xtask sudo-test -p kbf-driver-native --test slot_gpu
//! -- --test-threads=1 --nocapture`.

#![cfg(target_os = "macos")]

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kbf_driver_native::home;
use kbf_driver_native::network::{Isolation, Network};
use kbf_driver_native::slots::macos::{Slot, Slots};
use kbf_driver_native::slots::{LEASES_ROOT_MODE, Pool};
use support::pool::{Scratch, Users};

const PROBE_SOURCE: &str = include_str!("slot_gpu/gpu_probe.swift");
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const GRACE: Duration = Duration::from_secs(3);

/// The places a system daemon keeps files for a uid outside its folders.
const PROTECTED: [&str; 2] = [
    "/private/var/protected/sfanalytics",
    "/private/var/protected/trustd/private",
];
const MDS_MESSAGES: &str = "/private/var/db/mds/messages";

fn text(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The generated UID of the user `name`.
fn generated_uid(name: &str) -> String {
    let said = text(
        "/usr/bin/dscl",
        &[".", "-read", &format!("/Users/{name}"), "GeneratedUID"],
    );
    let guid = said
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(guid.len(), 36, "GeneratedUID of {name}: {said:?}");
    guid
}

/// The probe, built into a folder the pool user can run it from.
fn build_probe(into: &Path) -> PathBuf {
    std::fs::create_dir_all(into).expect("probe folder");
    std::fs::set_permissions(into, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let source = into.join("gpu_probe.swift");
    std::fs::write(&source, PROBE_SOURCE).expect("source");
    let program = into.join("gpu_probe");
    let built = Command::new("/usr/bin/swiftc")
        .arg("-O")
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .output()
        .expect("swiftc");
    assert!(
        built.status.success(),
        "swiftc: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    program
}

struct Lease {
    isolation: Isolation,
    dir: PathBuf,
    vars: Vec<(&'static str, OsString)>,
}

impl Lease {
    /// `program` as the slot's user under the slot profile, with the lease's variables
    /// and `extra` ones. It ends on its own or when the test ends the uid's processes.
    fn command(&self, slot: &Slot, extra: &[&str], program: &Path) -> Command {
        self.command_with(slot, &self.isolation, Network::Off, extra, (program, &[]))
    }

    /// `program` with `args` as the slot's user under `isolation` and the slot profile of
    /// `network`.
    fn command_with(
        &self,
        slot: &Slot,
        isolation: &Isolation,
        network: Network,
        extra: &[&str],
        (program, args): (&Path, &[&str]),
    ) -> Command {
        let mut inner: Vec<String> = Vec::new();
        for (name, value) in &self.vars {
            inner.push(format!("{name}={}", value.to_string_lossy()));
        }
        inner.push(format!("PATH={PATH}"));
        inner.extend(extra.iter().map(|e| (*e).to_owned()));
        inner.push(program.to_string_lossy().into_owned());
        inner.extend(args.iter().map(|a| (*a).to_owned()));
        let (launcher, args) = slot
            .launch(
                isolation,
                network,
                &self.dir,
                PathBuf::from("/usr/bin/env"),
                &inner,
            )
            .expect("launch");
        let mut command = Command::new(launcher);
        command
            .args(args)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null());
        command
    }
}

/// A lease directory the way the daemon makes one for `slot`.
async fn lease(scratch: &Scratch, slot: &Slot) -> Lease {
    let leases = scratch.leases();
    std::fs::create_dir_all(&leases).expect("leases");
    std::fs::set_permissions(&leases, std::fs::Permissions::from_mode(LEASES_ROOT_MODE))
        .expect("chmod");
    let dir = leases.join("lease-1-1");
    std::fs::create_dir(&dir).expect("lease dir");
    let vars = home::make(&dir).await.expect("home variables");
    slot.hand_over(&dir).await.expect("hand over");
    Lease {
        isolation: Isolation::detect(),
        dir: std::fs::canonicalize(&dir).expect("real path"),
        vars,
    }
}

/// Every path at or below `root` that `uid` owns. Never through a link.
fn owned_below(root: &Path, uid: u32, found: &mut Vec<PathBuf>) {
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        return;
    };
    if meta.uid() == uid {
        found.push(root.to_owned());
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
            owned_below(&entry.path(), uid, found);
        }
    }
}

/// What the uid owns in the system places the clean sweeps outside its own folders, and
/// in all of `/private/var/folders` except its entry's root and four folders, which are
/// the uid's by design (the test looks inside them on its own).
fn uid_files_outside(uid: u32, folders: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in PROTECTED.iter().map(Path::new).chain([
        Path::new("/private/var/db/mds"),
        Path::new("/private/var/folders"),
    ]) {
        owned_below(root, uid, &mut found);
    }
    found.retain(|path| {
        path != folders
            && !["0", "C", "T", "X"]
                .iter()
                .any(|name| path == &folders.join(name))
    });
    found
}

/// The processes now, pid to uid.
fn processes() -> BTreeMap<i32, u32> {
    text("/bin/ps", &["-ax", "-o", "pid=,uid="])
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect()
}

/// The I/O Registry's user clients created by a process of `uid`.
fn registry_clients_of(uid: u32) -> Vec<String> {
    const CREATOR: &str = "\"IOUserClientCreator\" = \"pid ";
    let processes = processes();
    text("/usr/sbin/ioreg", &["-l", "-w0"])
        .lines()
        .filter_map(|line| {
            let rest = &line[line.find(CREATOR)? + CREATOR.len()..];
            let pid: i32 = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            (processes.get(&pid) == Some(&uid)).then(|| line.trim().chars().take(160).collect())
        })
        .collect()
}

/// The entries of the `C` folder of the uid's `/var/folders` entry named like a Metal cache.
fn metal_caches(folders: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(folders.join("C"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("com.apple.metal"))
        })
        .collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// What the test puts where a framework import leaves it, and what it must take away
/// whether the test ends or panics: the planted files and folders, the process that holds
/// a device, and the probe folder.
struct Litter {
    uid: u32,
    /// Every planted file, whoever owns it.
    files: Vec<PathBuf>,
    /// What the uid owns of it, which the clean must remove.
    residue: Vec<PathBuf>,
    /// The folders the test made, deepest first, removed when empty.
    made: Vec<PathBuf>,
    holder: Option<std::process::Child>,
    probes: PathBuf,
}

impl Drop for Litter {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill")
            .args(["-KILL", "-U", &self.uid.to_string()])
            .status();
        if let Some(holder) = self.holder.as_mut() {
            let _ = holder.kill();
            let _ = holder.wait();
        }
        for file in &self.files {
            let _ = std::fs::remove_file(file);
        }
        for dir in &self.made {
            let _ = std::fs::remove_dir(dir);
        }
        let _ = std::fs::remove_dir_all(&self.probes);
    }
}

/// Makes `file` with the given owner and mode, and the folders above it that are missing.
/// The folders the test makes belong to `folder_owner`.
fn plant(file: &Path, owner: u32, folder_owner: u32, mode: u32, litter: &mut Litter) {
    let folder = file.parent().expect("parent");
    let mut at = folder;
    let first = litter.made.len();
    while !at.exists() {
        litter.made.push(at.to_owned());
        at = at.parent().expect("parent");
    }
    std::fs::create_dir_all(folder).expect("plant folder");
    for made in &litter.made[first..] {
        std::os::unix::fs::chown(made, Some(folder_owner), None).expect("chown folder");
    }
    litter.files.push(file.to_owned());
    std::fs::write(file, "x").expect("plant file");
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)).expect("chmod");
    std::os::unix::fs::chown(file, Some(owner), None).expect("chown");
}

/// The MLX arm: `Ok` with what the matrix product printed, or `Err` with why MLX could not
/// be set up on this runner (no `python3`, no wheel, no network), which the test prints
/// as `MLX_SKIPPED`. A product that fails once MLX is installed is a failure of the
/// profile, not a skip: it is returned as `Ok` with its output for the test to judge.
fn mlx_arm(slot: &Slot, lease: &Lease, venv: &Path, python: &Path) -> Result<String, String> {
    let ran = |network, program: &Path, args: &[&str]| {
        lease
            .command_with(slot, &lease.isolation, network, &[], (program, args))
            .output()
            .map_err(|e| e.to_string())
    };
    let step = |what: &str, network, program: &Path, args: &[&str]| {
        let out = ran(network, program, args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err.lines().rev().take(3).collect();
            Err(format!("{what}: {tail:?}"))
        }
    };
    step(
        "venv",
        Network::Off,
        Path::new("/usr/bin/python3"),
        &["-m", "venv", venv.to_str().unwrap()],
    )?;
    step(
        "pip install mlx",
        Network::On,
        python,
        &[
            "-m",
            "pip",
            "install",
            "--quiet",
            "--disable-pip-version-check",
            "mlx",
        ],
    )?;
    let product = ran(
        Network::Off,
        python,
        &[
            "-c",
            "import mlx.core as mx; a = mx.ones((256, 256)); b = a @ a; mx.eval(b); print('MLX_PASS', float(b[0, 0]))",
        ],
    )?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&product.stdout),
        String::from_utf8_lossy(&product.stderr)
    ))
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

/// Catches a clean that leaves the uid's GPU state to the next lease on its slot. After a
/// real Metal run (a device, a shader compiled at run time, a kernel) and a second Metal
/// process still holding its device:
///
/// - the probe's Metal caches are in the uid's `/var/folders` entry before the clean, so
///   the check below cannot pass on an empty entry;
/// - the registry lists a user client of the uid before the kill, so the registry check can
///   see one, and none after the clean;
/// - after the clean the slot is free (the clean found nothing left), the entry holds its
///   four empty folders and nothing else, the home is empty, and the uid owns nothing under
///   `/private/var/protected`, `/private/var/db/mds` or `/private/var/folders`;
/// - the uid's planted file in its security-message folder is gone (the sweep keeps that
///   folder, which is readable in the daemon's context);
/// - a file of the uid's that it could read in each protected store (planted when root can
///   write there) cannot be read by a lease under the slot profile, and the same read
///   outside the sandbox is printed as `PROTECTED-CONTROL`, so the deny is seen to be the
///   profile's when the store lets the uid in;
/// - the Metal probe passes under the slot profile with that deny, and so does an MLX
///   matrix product (`MLX_PASS`) when MLX installs on the runner (`MLX_SKIPPED` says why
///   not).
#[ignore = "needs root, real users, a Metal device and swiftc: see spike-slot-reach.yml job slot-gpu"]
#[tokio::test(flavor = "multi_thread")]
async fn a_pool_lease_that_used_the_gpu_leaves_none_of_it_after_the_clean() {
    support::pool::assert_root("slot_gpu");
    let scratch = Scratch::new("slotgpu");
    let users = Users::new("slotgpu", 1);
    let pool = Pool::new(users.0.clone()).expect("pool");
    // Not `std::env::temp_dir()`: root's own folder, which the pool user cannot enter.
    let probes = PathBuf::from("/private/tmp").join(format!("kbf-slot-gpu-{}", std::process::id()));
    let user = users.0[0].clone();
    let uid = user.uid;
    let mut litter = Litter {
        uid,
        files: Vec::new(),
        residue: Vec::new(),
        made: Vec::new(),
        holder: None,
        probes: probes.clone(),
    };
    let probe = build_probe(&probes);
    let guid = generated_uid(&user.name);

    let slots =
        Slots::start_with_grace(&pool, Duration::from_secs(20), GRACE, None).expect("slots");
    assert_eq!(
        slots.count(),
        1,
        "the clean at the start leaves the slot free"
    );
    let slot = slots.take().await.expect("a slot");
    let lease = lease(&scratch, &slot).await;
    let (home, folders) = (slot.ready().home.clone(), slot.ready().folders.clone());

    // The Metal arm, then a process that keeps its device open.
    let out = lease.command(&slot, &[], &probe).output().expect("probe");
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        said.contains("GPU_PROBE_PASS"),
        "the Metal probe: {said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let holding = probes.join("holding.out");
    litter.holder = Some(
        lease
            .command(&slot, &["GPU_HOLD=1"], &probe)
            .stdout(std::fs::File::create(&holding).expect("holder output"))
            .stderr(Stdio::null())
            .spawn()
            .expect("holder"),
    );
    assert!(
        wait_for(&holding, "HOLD_READY", 180),
        "the holder never held a device"
    );
    let caches = metal_caches(&folders);
    let clients = registry_clients_of(uid);

    // The deny: a file of the uid's, readable by it, in each protected store (planted when
    // root can write there; a store that refuses root holds nothing to read), which the
    // lease must not read under the slot profile.
    let mut denied = Vec::new();
    for store in PROTECTED {
        let file = Path::new(store).join(&guid).join("lease-readable");
        let folder = file.parent().expect("parent").to_owned();
        if std::fs::create_dir_all(&folder).is_err() {
            println!("PROTECTED-CONTROL {store}: root cannot make a folder here");
            continue;
        }
        plant(&file, uid, 0, 0o644, &mut litter);
        let cat = Path::new("/bin/cat");
        let outside = lease
            .command_with(
                &slot,
                &Isolation::None,
                Network::Off,
                &[],
                (cat, &[file.to_str().unwrap()]),
            )
            .output()
            .expect("cat");
        println!(
            "PROTECTED-CONTROL {store}: unsandboxed read {}",
            if outside.status.success() {
                "works"
            } else {
                "is refused too"
            }
        );
        let inside = lease
            .command_with(
                &slot,
                &lease.isolation,
                Network::Off,
                &[],
                (cat, &[file.to_str().unwrap()]),
            )
            .output()
            .expect("cat");
        denied.push((
            store,
            inside.status.success(),
            String::from_utf8_lossy(&inside.stderr).into_owned(),
        ));
    }

    // MLX under the profile, when it installs: a venv in the lease, mlx from the network
    // (the profile with the network on), then a matrix product on the GPU with it off.
    let venv = lease.dir.join("venv");
    let python = venv.join("bin/python");
    let mlx = mlx_arm(&slot, &lease, &venv, &python);

    // What a framework import makes in the security-message folder, planted as the system
    // makes it.
    let message = Path::new(MDS_MESSAGES)
        .join(uid.to_string())
        .join("se_SecurityMessages");
    plant(&message, uid, 0, 0o600, &mut litter);
    litter.residue.push(message);

    // The end of the lease as the driver makes it, the lease directory, the clean.
    let uid_arg = uid.to_string();
    let _ = Command::new("/usr/bin/pkill")
        .args(["-TERM", "-U", &uid_arg])
        .status();
    std::thread::sleep(GRACE);
    let _ = Command::new("/usr/bin/pkill")
        .args(["-KILL", "-U", &uid_arg])
        .status();
    if let Some(mut holder) = litter.holder.take() {
        let _ = holder.wait();
    }
    slot.reclaim(&lease.dir).await.expect("reclaim");
    std::fs::remove_dir_all(&lease.dir).expect("remove the lease directory");
    slot.release().await;

    // Everything is read before anything can end the test, then the plants are removed.
    let free = slots.count();
    let outside = uid_files_outside(uid, &folders);
    let residue_left: Vec<PathBuf> = (litter.residue.iter())
        .filter(|p| p.exists())
        .cloned()
        .collect();
    let left_in_entry: Vec<String> = names(&folders)
        .into_iter()
        .filter_map(|name| {
            let inside = names(&folders.join(&name));
            let skeleton = ["0", "C", "T", "X"].contains(&name.as_str());
            (!skeleton || !inside.is_empty()).then(|| format!("{name}: {inside:?}"))
        })
        .collect();
    let left_in_home = names(&home);
    let still = registry_clients_of(uid);
    drop(litter);

    assert!(
        !caches.is_empty(),
        "the Metal run left no cache in {}",
        folders.display()
    );
    assert!(
        !clients.is_empty(),
        "the registry shows no user client of uid {uid}"
    );
    for (store, readable, why) in &denied {
        assert!(
            !readable,
            "a lease under the slot profile read a file in {store}"
        );
        println!("PROTECTED-DENIED {store}: {}", why.trim());
    }
    match &mlx {
        Ok(said) => assert!(
            said.contains("MLX_PASS 256.0"),
            "an MLX matrix product under the slot profile: {said}"
        ),
        Err(why) => println!("MLX_SKIPPED {why}"),
    }
    assert_eq!(free, 1, "the clean left the slot dirty");
    assert!(
        residue_left.is_empty(),
        "planted residue survived: {residue_left:?}"
    );
    assert!(outside.is_empty(), "the uid still owns {outside:?}");
    assert!(
        left_in_entry.is_empty(),
        "the /var/folders entry holds {left_in_entry:?}"
    );
    assert!(left_in_home.is_empty(), "the home holds {left_in_home:?}");
    assert!(
        still.is_empty(),
        "the registry still has clients of the uid: {still:?}"
    );
}
