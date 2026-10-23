//! What a pool lease's GPU work leaves behind, and that the slot's clean removes it, on a
//! Mac with a Metal device (docs/design/macos-end-state.md, "GPU state of a pool lease").
//!
//! One lease on a pool of one user runs, under the slot profile with the lease's `HOME`
//! and `TMPDIR`, a Metal probe (`tests/slot_gpu/gpu_probe.swift`: a device, a shader
//! compiled at run time, a compute kernel), then a second copy that keeps its device
//! open, so that the I/O Registry has a user client of the uid. The lease ends as the
//! driver ends it, the lease directory goes, and the clean runs. The files that a
//! framework import (MLX) makes outside the user's folders, which the Metal probe does
//! not, are planted the way the system makes them: owned by the uid, in a folder named
//! by the user's generated UID, beside a file of root's that must stay.
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
    /// and `extra` ones, ended by an alarm after `secs`.
    fn command(&self, slot: &Slot, extra: &[&str], program: &Path, secs: u64) -> Command {
        let mut inner: Vec<String> = vec![
            "-e".into(),
            "alarm shift; exec @ARGV".into(),
            secs.to_string(),
            "/usr/bin/env".into(),
        ];
        for (name, value) in &self.vars {
            inner.push(format!("{name}={}", value.to_string_lossy()));
        }
        inner.push(format!("PATH={PATH}"));
        inner.extend(extra.iter().map(|e| (*e).to_owned()));
        inner.push(program.to_string_lossy().into_owned());
        let (launcher, args) = slot
            .launch(
                &self.isolation,
                Network::Off,
                &self.dir,
                PathBuf::from("/usr/bin/perl"),
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
/// in all of `/private/var/folders`.
fn uid_files_outside(uid: u32) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in PROTECTED.iter().map(Path::new).chain([
        Path::new("/private/var/db/mds"),
        Path::new("/private/var/folders"),
    ]) {
        owned_below(root, uid, &mut found);
    }
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

/// What the test puts where a framework import leaves it, and takes away at the end.
#[derive(Default)]
struct Planted {
    /// Files owned by the uid.
    residue: Vec<PathBuf>,
    /// The folders the test made, deepest first, removed when empty.
    made: Vec<PathBuf>,
}

/// Makes `file` with the given owner and mode, and the folders above it that are missing.
fn plant(file: &Path, owner: u32, mode: u32, planted: &mut Planted) {
    let folder = file.parent().expect("parent");
    let mut at = folder;
    while !at.exists() {
        planted.made.push(at.to_owned());
        at = at.parent().expect("parent");
    }
    std::fs::create_dir_all(folder).expect("plant folder");
    std::fs::write(file, "x").expect("plant file");
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)).expect("chmod");
    std::os::unix::fs::chown(file, Some(owner), None).expect("chown");
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
/// - the uid's planted files in the analytics and trust stores and its security-message
///   folder are gone, and root's file in the same folder stays (the sweep removes by
///   owner, not by folder).
#[ignore = "needs root, real users, a Metal device and swiftc: see spike-slot-reach.yml job slot-gpu"]
#[tokio::test(flavor = "multi_thread")]
async fn a_pool_lease_that_used_the_gpu_leaves_none_of_it_after_the_clean() {
    support::pool::assert_root("slot_gpu");
    let scratch = Scratch::new("slotgpu");
    let users = Users::new("slotgpu", 1);
    let pool = Pool::new(users.0.clone()).expect("pool");
    // Not `std::env::temp_dir()`: root's own folder, which the pool user cannot enter.
    let probes = PathBuf::from("/private/tmp").join(format!("kbf-slot-gpu-{}", std::process::id()));
    let probe = build_probe(&probes);
    let user = users.0[0].clone();
    let uid = user.uid;
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
    let out = lease
        .command(&slot, &[], &probe, 300)
        .output()
        .expect("probe");
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        said.contains("GPU_PROBE_PASS"),
        "the Metal probe: {said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let holding = probes.join("holding.out");
    let mut holder = lease
        .command(&slot, &["GPU_HOLD=1"], &probe, 600)
        .stdout(std::fs::File::create(&holding).expect("holder output"))
        .stderr(Stdio::null())
        .spawn()
        .expect("holder");
    assert!(
        wait_for(&holding, "HOLD_READY", 180),
        "the holder never held a device"
    );
    let caches = metal_caches(&folders);
    let clients = registry_clients_of(uid);

    // What an MLX import makes outside the folders, planted as the system makes it.
    let mut planted = Planted::default();
    let control = Path::new(PROTECTED[0]).join(&guid).join("root-owned");
    plant(&control, 0, 0o600, &mut planted);
    for file in [
        Path::new(PROTECTED[0]).join(&guid).join("events.db"),
        Path::new(PROTECTED[1]).join(&guid).join("trust.db"),
        Path::new(MDS_MESSAGES)
            .join(uid.to_string())
            .join("se_SecurityMessages"),
    ] {
        plant(&file, uid, 0o600, &mut planted);
        planted.residue.push(file);
    }

    // The end of the lease as the driver makes it, the lease directory, the clean.
    let uid_arg = uid.to_string();
    let _ = Command::new("/usr/bin/pkill")
        .args(["-TERM", "-U", &uid_arg])
        .status();
    std::thread::sleep(GRACE);
    let _ = Command::new("/usr/bin/pkill")
        .args(["-KILL", "-U", &uid_arg])
        .status();
    let _ = holder.wait();
    slot.reclaim(&lease.dir).await.expect("reclaim");
    std::fs::remove_dir_all(&lease.dir).expect("remove the lease directory");
    slot.release().await;

    // Everything is read before anything can end the test, then the plants are removed.
    let free = slots.count();
    let outside = uid_files_outside(uid);
    let control_stays = control.exists();
    let residue_left: Vec<&PathBuf> = planted.residue.iter().filter(|p| p.exists()).collect();
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
    let _ = std::fs::remove_file(&control);
    for file in &planted.residue {
        let _ = std::fs::remove_file(file);
    }
    for dir in &planted.made {
        let _ = std::fs::remove_dir(dir);
    }
    let _ = std::fs::remove_dir_all(&probes);

    assert!(
        !caches.is_empty(),
        "the Metal run left no cache in {}",
        folders.display()
    );
    assert!(
        !clients.is_empty(),
        "the registry shows no user client of uid {uid}"
    );
    assert_eq!(free, 1, "the clean left the slot dirty");
    assert!(
        residue_left.is_empty(),
        "planted residue survived: {residue_left:?}"
    );
    assert!(
        control_stays,
        "the sweep removed root's file in the uid's folder"
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
