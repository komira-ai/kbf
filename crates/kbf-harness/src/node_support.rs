//! What a farm of nodes running the container driver over the stand-in `crun` needs on
//! disk: the stand-in and the image it names, unpacked ([`crun`]), the planted cgroup tree
//! ([`cgroup_tree`]) and id files ([`id_files`]), and the check of what is on a node's
//! disk ([`files_holding`]). Shared by this crate's farm, kbf-all-in-one's tests/binary.rs
//! and kbf-cli's tests (their `support/node.rs` re-exports it).

use std::path::{Path, PathBuf};

/// The per-architecture manifest whose digest names the image the node's store holds.
pub const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

/// `--id-files`: a passwd naming this test's user `kbf`, with 65,536 subordinate ids.
pub fn id_files(dir: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let ids = dir.join("ids");
    std::fs::create_dir_all(&ids).expect("mkdir");
    let passwd = ids.join("passwd");
    std::fs::write(&passwd, "").expect("write");
    let uid = std::fs::metadata(&passwd).expect("stat").uid();
    std::fs::write(&passwd, format!("kbf:x:{uid}:{uid}::/:/usr/bin/false\n")).expect("write");
    for file in ["subuid", "subgid"] {
        std::fs::write(ids.join(file), "kbf:100000:65536\n").expect("write");
    }
    ids
}

/// Where tests that run a container daemon put their directories: `target`, the per-target
/// temporary directory a test binary names (`CARGO_TARGET_TMPDIR`, which only it can name),
/// unless an ancestor of it is closed to other users (a home directory often is), which the
/// container driver refuses (its containers' ids are not the daemon's, and must be able to
/// search down to the data directory); then the system's temporary directory.
pub fn scratch_root(target: PathBuf) -> PathBuf {
    if open_to_others(&target) {
        target
    } else {
        std::env::temp_dir()
    }
}

/// Whether every directory above `path` (and `path` itself) can be searched by users other
/// than its owner.
fn open_to_others(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::canonicalize(path).is_ok_and(|real| {
        real.ancestors()
            .all(|a| std::fs::metadata(a).is_ok_and(|m| m.permissions().mode() & 0o001 != 0))
    })
}

/// The stand-in `crun` in `bin` (a link to `fake`, the package's build of
/// crates/kbf-driver-container/tests/bin/fake_crun.rs), `kbf-act` beside it, the id tools
/// the OCI backend's start-up looks for on `PATH` (setuid files, as `uidmap` installs
/// them) and a `podman` that fails and records that it ran (the same program as the
/// stand-in: [`podman_ran`]), with the image [`MANIFEST`] names already unpacked in the
/// node's image store under `data` (the node's `--data-dir`); the stand-in runs `steps`
/// as each action. Returns the image as an action names it.
pub fn crun(bin: &Path, data: &Path, fake: &str, steps: &[&str]) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let state = bin.join("crun-state");
    std::fs::create_dir_all(&state).expect("mkdir");
    std::os::unix::fs::symlink(fake, bin.join("crun")).expect("link crun");
    std::os::unix::fs::symlink(fake, bin.join("podman")).expect("link podman");
    for tool in ["newuidmap", "newgidmap"] {
        let path = bin.join(tool);
        std::fs::write(&path, b"").expect("write the tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).expect("chmod");
    }
    kbf_testkit::act::link_self(&bin.join(kbf_testkit::act::NAME)).expect("link kbf-act");
    std::fs::write(state.join("nonce"), format!("{}:", std::process::id())).expect("nonce");
    let knob: String = steps.iter().map(|s| format!("{s}\n")).collect();
    std::fs::write(state.join("action"), knob).expect("action");
    let hex = kbf_transfer::digest_of(MANIFEST).hash;
    let rootfs = data.join("leases/oci-images").join(&hex).join("rootfs");
    std::fs::create_dir_all(rootfs.join("kbf/root")).expect("an unpacked image");
    format!("docker://registry.test/tools/busybox@sha256:{hex}")
}

/// The flag that points the daemon at the stand-in `crun` [`crun`] put in `bin`.
pub fn oci_runtime(bin: &Path) -> String {
    format!("--oci-runtime={}", bin.join("crun").display())
}

/// What the `podman` of `bin` recorded: nothing, on a daemon that never runs it.
pub fn podman_ran(bin: &Path) -> String {
    std::fs::read_to_string(bin.join("podman-ran")).unwrap_or_default()
}

/// A cgroup v2 mount of plain directories at `bin/cgroup`, where the stand-in `crun`
/// looks, holding the daemon's unit; the flags that point the daemon at it.
pub fn cgroup_tree(bin: &Path) -> Vec<String> {
    let root = bin.join("cgroup");
    let unit = root.join("kbf.slice/kbf-all-in-one.service");
    std::fs::create_dir_all(&unit).expect("mkdir");
    let plant = |path: PathBuf, text: &str| std::fs::write(path, text).expect("plant");
    plant(
        root.join("cgroup.controllers"),
        "cpuset cpu io memory pids\n",
    );
    plant(unit.join("cgroup.controllers"), "cpu memory pids\n");
    plant(unit.join("cgroup.procs"), "1\n");
    plant(unit.join("cpuset.cpus.effective"), "0\n");
    let own = bin.join("self-cgroup");
    plant(own.clone(), "0::/kbf.slice/kbf-all-in-one.service\n");
    vec![
        format!("--cgroup-root={}", root.display()),
        format!("--self-cgroup={}", own.display()),
    ]
}

/// `PATH` with `bin`, where [`crun`] put the id tools and the failing `podman`, first.
pub fn image_path(bin: &Path) -> String {
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// An input of more bytes than any chunk or buffer of the read path (a ByteStream chunk
/// and the input cache's copy buffer are 1 MiB), none of whose 64-byte runs repeats.
pub fn input_bytes() -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..(3 << 20) + 7)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}

/// Every file under `dirs` (recursively, not following symlinks) that holds `bytes`
/// whole, each inode once: its path and size.
pub fn files_holding(dirs: &[PathBuf], bytes: &[u8]) -> Vec<(PathBuf, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let mut found = std::collections::BTreeMap::new();
    let mut todo: Vec<PathBuf> = dirs.to_vec();
    while let Some(at) = todo.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            continue;
        };
        if meta.is_dir() {
            let listed = std::fs::read_dir(&at).into_iter().flatten().flatten();
            todo.extend(listed.map(|entry| entry.path()));
            continue;
        }
        // A regular file at least as long as `bytes` (a bitwise `&`: one condition, not two).
        let big_enough = meta.is_file() & (meta.len() >= bytes.len() as u64);
        if !big_enough {
            continue;
        }
        let held = std::fs::read(&at).unwrap_or_default();
        let anchor = &bytes[..64];
        let holds = held
            .windows(64)
            .enumerate()
            .filter(|(_, run)| *run == anchor)
            .any(|(at, _)| held.get(at..at + bytes.len()) == Some(bytes));
        if holds {
            found
                .entry((meta.dev(), meta.ino()))
                .or_insert((at, meta.len()));
        }
    }
    found.into_values().collect()
}

#[cfg(test)]
#[path = "node_support_tests.rs"]
mod tests;
