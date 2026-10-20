//! The first process of an action's container (`init/kbf_init.rs`), embedded in the
//! driver: how it is put on the node and where the container sees it.
//!
//! The binary is built statically by `build.rs`, so it runs in any image. The driver
//! writes it to `<scratch>/oci-bin/kbf-init` (the scratch directory is searchable by
//! others; an installed or built daemon's directory may not be) and binds it read-only
//! at [`DEST`], in the container's own `/dev` (a `tmpfs` made fresh per container, so
//! the image's root is never written to).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// The program, empty on a target that runs no containers.
const BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kbf-init"));

/// Where the container sees it.
pub(crate) const DEST: &str = "/dev/kbf-init";

/// The file in a lease directory the init writes why the program could not be started
/// to (its stdin: the container's other processes cannot reach it).
pub(crate) fn report_path(lease: &Path) -> PathBuf {
    lease.join("init-report")
}

/// Makes the report file of the lease in `lease`, empty, for the init to write to.
pub(crate) fn create_report(lease: &Path) -> Result<std::fs::File, String> {
    let path = report_path(lease);
    std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the program into `<scratch>/oci-bin/kbf-init` (mode 0755, replaced
/// atomically) and returns that path.
pub(crate) fn install(scratch: &Path) -> std::io::Result<PathBuf> {
    install_bytes(scratch, BINARY)
}

fn install_bytes(scratch: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    if bytes.is_empty() {
        return Err(std::io::Error::other(
            "this build has no container init: containers run on Linux only",
        ));
    }
    let dir = scratch.join("oci-bin");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let staged = dir.join(format!(".kbf-init-{}", std::process::id()));
    std::fs::write(&staged, bytes)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    let target = dir.join("kbf-init");
    std::fs::rename(&staged, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join(format!("init-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Catches an init the container's user cannot execute (a mode without the others'
    /// bits: the action's ids are not the daemon's), one that is not the bytes built, a
    /// second install failing on the first's file, and a build without an init (a target
    /// that runs no containers) installing an empty file the container would fail to
    /// start on.
    #[test]
    fn the_init_is_installed_executable_by_others_and_replaced_atomically() {
        let scratch = scratch("install");
        let first = install(&scratch).expect("install");
        assert_eq!(first, scratch.join("oci-bin/kbf-init"));
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&first), 0o755);
        assert_eq!(mode(first.parent().expect("dir")), 0o755);
        assert_eq!(std::fs::read(&first).expect("read"), BINARY);
        assert!(!BINARY.is_empty());
        assert_eq!(install(&scratch).expect("again"), first);
        let names: Vec<_> = std::fs::read_dir(first.parent().expect("dir"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["kbf-init"], "no staged file left");
        // The report file: empty each time, and an error naming the file when the lease
        // directory is not there.
        let lease = scratch.join("lease");
        std::fs::create_dir(&lease).expect("lease");
        std::fs::write(report_path(&lease), b"left over").expect("old report");
        create_report(&lease).expect("report");
        assert_eq!(std::fs::read(report_path(&lease)).expect("read"), b"");
        let why = create_report(&scratch.join("gone")).expect_err("no lease directory");
        assert!(why.contains("gone/init-report: "), "{why}");
        let empty = install_bytes(&scratch, b"").expect_err("empty");
        assert!(empty.to_string().contains("no container init"), "{empty}");
    }
}
