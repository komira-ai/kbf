//! The driver's [`VmSpec`] as `kbf-vmm vm-run` reads it: a [`kbf_vmm::spec::VmSpec`] in a
//! JSON file in the clone's directory.
//!
//! What the driver decided maps one to one: the clone is the bundle, the booked cores
//! and memory are the CPU and memory sizes (memory in whole MiB), and the two shares are
//! tagged shares the guest mounts itself. Always a graphics device: a macOS guest with
//! none crash-loops its window server, and UI tests need the session. Not carried: the
//! machine identifier (the spec has no field for one) and a network device (the spec has
//! none, so a spec that asks for one is refused, not started without).

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use kbf_vmm::spec::{TaggedShare, VmSpec as VmmSpec};

use crate::host::VmSpec;

/// The file in the clone's directory holding the spec.
pub const SPEC_FILE: &str = "vm-spec.json";

const MIB: u64 = 1 << 20;

/// `spec` as `vm-run` reads it.
///
/// # Errors
/// The spec asks for a network device, which `vm-run` cannot give, or for no memory.
pub fn translate(spec: &VmSpec) -> io::Result<VmmSpec> {
    if !spec.network.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kbf-vmm has no network device; a VM that asks for one is not started",
        ));
    }
    let memory_mib = spec.memory_bytes / MIB;
    if memory_mib == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a VM needs at least 1 MiB of memory",
        ));
    }
    Ok(VmmSpec {
        bundle: spec.clone.clone(),
        cpus: spec.cpus,
        memory_mib,
        graphics: true,
        automount_dir: None,
        tagged: spec
            .shares
            .iter()
            .map(|share| TaggedShare {
                tag: share.tag.to_owned(),
                path: share.host.clone(),
                multiple_name: None,
                read_only: share.read_only,
            })
            .collect(),
        vsock_ports: Vec::new(),
        console_ports: Vec::new(),
        results_dir: None,
    })
}

/// Writes `spec` as `vm-run` reads it into the clone's directory and returns the file's
/// path. The file is new (a spec is never overwritten, nor written through a link) and
/// readable by the VM user.
///
/// # Errors
/// See [`translate`]; or the file cannot be written.
pub fn write(spec: &VmSpec) -> io::Result<PathBuf> {
    let json = serde_json::to_vec_pretty(&translate(spec)?).map_err(io::Error::other)?;
    let path: &Path = &spec.clone;
    let file = path.join(SPEC_FILE);
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&file)?;
    out.write_all(&json)?;
    out.sync_all()?;
    Ok(file)
}
