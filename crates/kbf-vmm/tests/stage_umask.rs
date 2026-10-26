//! `stage::execute` under a restrictive umask. This is its own test binary (one test, so one
//! process): `umask` is process-wide and would change the files other tests create.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use kbf_vmm::stage::{Env, Plan, execute, remove_path};

/// Catches a directory made for the volume (and the manifest) whose mode depends on the
/// umask of the host process: under 077 each must still be 0755 (0644 for the file).
#[test]
fn directories_made_on_the_volume_have_their_modes_whatever_the_umask_is() {
    let volume = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stage-umask");
    let _ = std::fs::remove_dir_all(&volume);
    std::fs::create_dir_all(&volume).expect("volume");
    let (free, clone) = (|_: &Path| Ok(u64::MAX), |_: &Path, _: &Path| Ok(()));
    let chown = |_: &Path| Ok(());
    let env = Env {
        free_bytes: &free,
        clone_tree: &clone,
        chown_root: &chown,
        remove: &remove_path,
    };
    // SAFETY: umask only sets this process's file creation mask; nothing else runs here.
    let before = unsafe { libc::umask(0o077) };
    let out = execute(&volume, &Plan::default(), &env);
    // SAFETY: as above.
    unsafe { libc::umask(before) };
    out.expect("execute");
    let mode = |rel: &str| {
        std::fs::metadata(volume.join(rel))
            .expect(rel)
            .permissions()
            .mode()
            & 0o7777
    };
    for dir in [
        "private",
        "private/var",
        "private/var/kbf-sim",
        "private/var/kbf-sim/runtimes",
    ] {
        assert_eq!(mode(dir), 0o755, "{dir}");
    }
    assert_eq!(mode("private/var/kbf-sim/staged.json"), 0o644);
}
