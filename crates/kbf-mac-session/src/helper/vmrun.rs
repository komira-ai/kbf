//! What `role-run` may start: the installed `kbf-vmm` as `vm-run --spec <file>`, and
//! nothing else.
//!
//! The caller (the daemon's account) names no program: the argument vector must be
//! exactly `<vmm> vm-run --spec <file>`, with `<vmm>` the helper's configured `--vmm`
//! path (a root-owned install the caller cannot change) and `<file>` a path inside the
//! images directory. The file is checked the way the image verbs check their bundles:
//! the path is compared by components (no `..`, no lookalike sibling of the
//! directory), and then walked from the images directory one name at a time without
//! following a link; every directory on the way must be a directory and the file a
//! regular file with a single link. Refusing a link here keeps a spec that was swapped for a
//! link to a file elsewhere from being named; the check is made once, before the start,
//! and `kbf-vmm` opens the file as the VM user, so what it reads is what that user could
//! read anyway.
//!
//! The check limits where the spec is, not what it says: a caller that can write the file
//! after the check (a daemon-owned directory inside the images directory) can have
//! `kbf-vmm` open any disk the VM user can read. That is a power the VM user's own
//! files already give, not a way to root.
//!
//! The caller passes no environment: `kbf-vmm` needs none, and a name such as
//! `DYLD_INSERT_LIBRARIES` or `PATH` would reach a process that runs as the VM user.
//! [`no_environment`] refuses a request that carries any.

use std::os::fd::OwnedFd;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, openat, statat};

use super::Helper;

/// The subcommand of `kbf-vmm` a `role-run` starts.
pub const VM_RUN: &str = "vm-run";
/// Its one flag.
pub const SPEC_FLAG: &str = "--spec";

impl Helper {
    /// Checks that `argv` is `<vmm> vm-run --spec <file>`; see the module documentation.
    ///
    /// # Errors
    /// Why `argv` is not.
    pub(super) fn check_vm_run(&self, argv: &[String]) -> Result<(), String> {
        let vmm = &self.settings.image.vmm;
        let [program, verb, flag, spec] = argv else {
            return Err(format!(
                "role-run starts only {} {VM_RUN} {SPEC_FLAG} <file>; got {} arguments",
                vmm.display(),
                argv.len()
            ));
        };
        if Some(program.as_str()) != vmm.to_str() || verb != VM_RUN || flag != SPEC_FLAG {
            return Err(format!(
                "role-run starts only {} {VM_RUN} {SPEC_FLAG} <file>",
                vmm.display()
            ));
        }
        check_spec(&self.settings.image.dir, spec)
    }
}

/// Refuses a `role-run` that carries environment variables: the process gets only the
/// ones the helper sets (`HOME`, `USER`, `LOGNAME`).
///
/// # Errors
/// Names the first variable (its name only).
pub(super) fn no_environment(env: &[(String, String)]) -> Result<(), String> {
    match env.first() {
        None => Ok(()),
        Some((name, _)) => Err(format!(
            "role-run takes no environment; got {} variable(s), the first {name:?}",
            env.len()
        )),
    }
}

/// Whether `spec` is a regular file with one link, directly or deeper inside `images`,
/// reached through no link.
fn check_spec(images: &Path, spec: &str) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("the spec {spec}: {why}"));
    let Ok(below) = Path::new(spec).strip_prefix(images) else {
        return refuse(&format!(
            "not inside the images directory {}",
            images.display()
        ));
    };
    let mut names = Vec::new();
    for component in below.components() {
        match component {
            Component::Normal(name) => names.push(name),
            _ => return refuse("the path goes up"),
        }
    }
    let Some((file, parents)) = names.split_last() else {
        return refuse("it is the images directory itself");
    };
    let open = |dir: &OwnedFd, name: &std::ffi::OsStr, flags: OFlags| {
        openat(
            dir,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
    };
    let mut dir = openat(
        CWD,
        images,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|why| format!("the images directory {}: {why}", images.display()))?;
    for name in parents {
        dir = match open(&dir, name, OFlags::RDONLY | OFlags::DIRECTORY) {
            Ok(next) => next,
            Err(why) => return refuse(&format!("{}: {why}", Path::new(name).display())),
        };
    }
    let stat = match statat(&dir, *file, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(why) => return refuse(&format!("{}: {why}", Path::new(file).display())),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return refuse("not a regular file with a single link");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
