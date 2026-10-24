//! The image verbs, `image-setup` and `image-harvest`: the two root-only steps of
//! building a guest image, run by the helper so the daemon (not a person with `sudo`)
//! can drive them.
//!
//! - `image-setup <bundle>` runs `kbf-vmm image setup --base <bundle> --guest-binary
//!   <guest>`, which attaches the bundle's disk image and writes the guest's first-boot
//!   files into its Data volume. Afterwards the bundle belongs to the standing VM user
//!   again and its files, and the bundle, carry the user-immutable flag (`uchg`).
//! - `image-harvest <bundle> <out>` runs `kbf-vmm image harvest --bundle <bundle> --out
//!   <out>/files`, which attaches the disk read-only and copies a fixed allowlist out.
//!   The helper makes `<out>` first: root's, mode 0700, new.
//!
//! What the caller cannot choose: the program (`--vmm`, an installed root-owned file),
//! the guest binary (`--guest-binary`, the same), every flag and every argument but the
//! two paths, and where those paths lead. A path is accepted only as a name directly
//! inside the images directory (`--images-dir`): the helper compares components, so
//! `..`, a longer path and a lookalike sibling are refused, and then builds the path it
//! passes on from the configured directory and that one name, never from the caller's
//! text.
//!
//! The bundle is taken before `kbf-vmm` runs, because the VM user made it and could
//! swap what root is about to attach. The helper opens the name without following a
//! link, makes the directory root's and mode 0700 through the descriptor, checks that
//! the name still leads to that directory, and makes every file in it root's, refusing
//! anything but plain single-link files (a symlink, a hard link to a file elsewhere, a
//! subdirectory). The images directory is the helper's own and either not group-writable
//! or sticky, so once the bundle is root's, the VM user cannot rename it away. After the
//! run the owners and the mode go back (to the VM user for `image-setup`, to what they
//! were for `image-harvest`), also when `kbf-vmm` failed.
//!
//! One image verb runs at a time. The ledger's lock is not held: `kbf-vmm` takes minutes.

use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{MutexGuard, PoisonError};

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Gid, Mode, OFlags, RawMode, Stat, Uid, chownat, fchmod, fchown,
    fstat, mkdirat, openat, statat, unlinkat,
};

use super::Helper;
use crate::autologin::CommandOutput;
use crate::proto::IMAGE_REPORT_BYTES;
use crate::start::owned_privately;

/// Sets the user-immutable flag on the paths it is given.
pub const CHFLAGS: &str = "/usr/bin/chflags";

/// What `kbf-vmm harvest` creates inside the helper's `<out>`.
const HARVEST_FILES: &str = "files";

/// Where the image verbs act, and the installed programs they use.
#[derive(Clone, Debug)]
pub struct ImageSettings {
    /// The images directory: bundles and harvest directories are names directly in it.
    pub dir: PathBuf,
    /// The `kbf-vmm` to run.
    pub vmm: PathBuf,
    /// The `kbf-guest` for macOS that `image-setup` has `kbf-vmm` copy into the guest.
    pub guest: PathBuf,
}

impl ImageSettings {
    /// The paths a test uses: everything under `dir`.
    #[cfg(test)]
    pub(crate) fn under(dir: &Path) -> Self {
        Self {
            dir: dir.join("images"),
            vmm: dir.join("kbf-vmm"),
            guest: dir.join("kbf-guest"),
        }
    }

    /// The one name `path` has directly inside the images directory.
    ///
    /// # Errors
    /// The path is not absolute text under the images directory, goes up (`..`), or is
    /// deeper than one name.
    fn name(&self, path: &str) -> Result<String, String> {
        if path.contains('\0') {
            return Err("a path holds NUL".to_owned());
        }
        let Ok(rest) = Path::new(path).strip_prefix(&self.dir) else {
            return Err(format!(
                "{path} is not inside the images directory {}",
                self.dir.display()
            ));
        };
        let mut parts = rest.components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(name)), None) => Ok(name.to_string_lossy().into_owned()),
            (Some(Component::ParentDir), _) | (_, Some(Component::ParentDir)) => {
                Err(format!("{path} goes up with `..`"))
            }
            _ => Err(format!(
                "{path} is not a name directly inside the images directory {}",
                self.dir.display()
            )),
        }
    }

    /// Opens the images directory, checked: the helper's own, not writable by others, and
    /// group-writable only with the sticky bit.
    fn open(&self) -> Result<OwnedFd, String> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let here = self.dir.display();
        // The configured path may go through a link (`/var` is one); the names below it
        // are opened without following any.
        let dir = openat(CWD, &self.dir, flags, Mode::empty())
            .map_err(at(&format!("opening the images directory {here}")))?;
        let stat = fstat(&dir).map_err(at(&format!("{here}")))?;
        shared_safely(&stat).map_err(at(&format!("the images directory {here}")))?;
        Ok(dir)
    }

    /// `path` as text for an argument.
    fn text(path: &Path) -> Result<&str, String> {
        path.to_str()
            .ok_or_else(|| format!("{} is not UTF-8", path.display()))
    }
}

/// Whether the images directory can be trusted: root's, not writable by others, and
/// group-writable only if sticky.
fn shared_safely(stat: &Stat) -> Result<(), String> {
    let me = rustix::process::geteuid().as_raw();
    let mode = stat.st_mode;
    if stat.st_uid != me {
        return Err(format!(
            "owned by uid {}, not the helper's uid {me}",
            stat.st_uid
        ));
    }
    if mode & 0o002 != 0 {
        return Err("writable by others".to_owned());
    }
    if mode & 0o020 != 0 && mode & 0o1000 == 0 {
        return Err("writable by its group without the sticky bit".to_owned());
    }
    Ok(())
}

/// A program the helper runs or reads: a regular file, the helper's own, not writable by
/// anyone else. Followed through links (the kit's `current` is one).
fn trusted_file(path: &Path) -> Result<(), String> {
    let here = path.display();
    let file = openat(CWD, path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(at(&format!("opening {here}")))?;
    let stat = fstat(&file).map_err(at(&format!("{here}")))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(format!("{here} is not a regular file"));
    }
    owned_privately(&stat).map_err(at(&format!("{here}")))
}

/// Who owns a file, and its mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    uid: u32,
    gid: u32,
    mode: RawMode,
}

impl Owner {
    fn of(stat: &Stat) -> Self {
        Self {
            uid: stat.st_uid,
            gid: stat.st_gid,
            mode: stat.st_mode & 0o7777,
        }
    }
}

/// Checks that `now`, the name looked up again after the directory was taken, is the
/// directory `held` was opened as and is the helper's.
fn check_claim(name: &str, held: &Stat, now: &Stat) -> Result<(), String> {
    let same = (held.st_dev, held.st_ino) == (now.st_dev, now.st_ino);
    if same && now.st_uid == rustix::process::geteuid().as_raw() {
        return Ok(());
    }
    Err(format!("{name} changed while it was being taken"))
}

/// `what` and the error, for `map_err`: `"<what>: <error>"`.
fn at<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> String {
    let what = what.to_owned();
    move |why| format!("{what}: {why}")
}

/// Logs a failure of a step that has no one to report to (giving a bundle back after the
/// verb already failed).
fn warn_if_failed(what: &str, result: Result<(), String>) {
    if let Err(why) = result {
        tracing::warn!("{what}: {why}");
    }
}

/// A bundle directory taken for root: its descriptor, and the owners it had.
struct Claim {
    dir: OwnedFd,
    was: Owner,
    /// Every file in it, with its owner before the claim, in name order.
    files: Vec<(String, Owner)>,
}

impl Claim {
    /// Takes the directory `name` in `images`, never through a link.
    ///
    /// # Errors
    /// `name` is not a plain directory, changed while it was taken, or holds anything but
    /// plain single-link files. Whatever was taken is given back.
    fn take(images: &OwnedFd, name: &str) -> Result<Self, String> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let dir = openat(images, name, flags, Mode::empty()).map_err(at(&format!(
            "{name} is not a plain directory in the images directory"
        )))?;
        let held = fstat(&dir).map_err(at(name))?;
        let mut claim = Self {
            dir,
            was: Owner::of(&held),
            files: Vec::new(),
        };
        if let Err(why) = claim.fill(images, name, &held) {
            warn_if_failed(
                &format!("giving {name} back after a refused claim"),
                claim.give_back(None),
            );
            return Err(why);
        }
        Ok(claim)
    }

    fn fill(&mut self, images: &OwnedFd, name: &str, held: &Stat) -> Result<(), String> {
        let (uid, gid) = (
            Uid::from_raw(rustix::process::geteuid().as_raw()),
            Gid::from_raw(rustix::process::getegid().as_raw()),
        );
        fchown(&self.dir, Some(uid), Some(gid)).map_err(at(&format!("taking {name}")))?;
        fchmod(&self.dir, Mode::from_raw_mode(0o700)).map_err(at(&format!("locking {name}")))?;
        let now = statat(images, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(at(&format!("looking {name} up again")))?;
        check_claim(name, held, &now)?;
        let listed = Dir::read_from(&self.dir).map_err(at(&format!("listing {name}")))?;
        let mut names = Vec::new();
        for entry in listed {
            let entry = entry.map_err(at(&format!("listing {name}")))?;
            let file = entry.file_name().to_string_lossy().into_owned();
            if file != "." && file != ".." {
                names.push(file);
            }
        }
        for file in names {
            let stat = statat(&self.dir, &file, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(at(&format!("{name}/{file}")))?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1
            {
                return Err(format!(
                    "{name}/{file} is not a plain file with one link; a bundle holds only those"
                ));
            }
            chownat(
                &self.dir,
                &file,
                Some(uid),
                Some(gid),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(at(&format!("taking {name}/{file}")))?;
            self.files.push((file, Owner::of(&stat)));
        }
        self.files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(())
    }

    /// The owner to give `was` back to: `to` if given, else what it was.
    fn owner(was: Owner, to: Option<(u32, u32)>) -> (Uid, Gid) {
        let (uid, gid) = to.unwrap_or((was.uid, was.gid));
        (Uid::from_raw(uid), Gid::from_raw(gid))
    }

    /// Gives the files back: to `to`, or to the owners they had.
    fn give_files(&self, to: Option<(u32, u32)>) -> Result<(), String> {
        for (file, was) in &self.files {
            let (uid, gid) = Self::owner(*was, to);
            chownat(
                &self.dir,
                file,
                Some(uid),
                Some(gid),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(at(&format!("giving {file} back")))?;
        }
        Ok(())
    }

    /// Gives the directory back, with the mode it had.
    fn give_dir(&self, to: Option<(u32, u32)>) -> Result<(), String> {
        let (uid, gid) = Self::owner(self.was, to);
        fchown(&self.dir, Some(uid), Some(gid)).map_err(at("giving the bundle back"))?;
        fchmod(&self.dir, Mode::from_raw_mode(self.was.mode))
            .map_err(at("restoring the bundle's mode"))
    }

    /// Gives everything back, files first (the directory is root's and closed to the
    /// others until the last step).
    fn give_back(&self, to: Option<(u32, u32)>) -> Result<(), String> {
        self.give_files(to)?;
        self.give_dir(to)
    }

    /// The names of the files.
    fn names(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|(file, _)| file.as_str())
    }
}

/// The reply's report: what `kbf-vmm` printed, cut at [`IMAGE_REPORT_BYTES`] on a
/// character boundary.
fn report(stdout: &str) -> String {
    let mut end = stdout.len().min(IMAGE_REPORT_BYTES);
    while !stdout.is_char_boundary(end) {
        end -= 1;
    }
    stdout[..end].to_owned()
}

impl Helper {
    fn one_image_job(&self) -> MutexGuard<'_, ()> {
        self.images.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs the installed `kbf-vmm` with `args`; its report when it exits 0.
    fn run_vmm(&self, args: &[&str]) -> Result<String, String> {
        let vmm = ImageSettings::text(&self.settings.image.vmm)?;
        let output: CommandOutput = self
            .host
            .command(vmm, args)
            .map_err(at("running kbf-vmm"))?;
        if output.success {
            return Ok(report(&output.stdout));
        }
        Err(format!(
            "kbf-vmm failed: {} {}",
            report(output.stderr.trim()),
            report(output.stdout.trim()),
        ))
    }

    /// `chflags -h uchg` on `paths`.
    fn flag_immutable(&self, paths: &[PathBuf]) -> Result<(), String> {
        let mut args = vec!["-h", "uchg"];
        for path in paths {
            args.push(ImageSettings::text(path)?);
        }
        let output = self
            .host
            .command(CHFLAGS, &args)
            .map_err(at("running chflags"))?;
        if output.success {
            return Ok(());
        }
        Err(format!("chflags failed: {}", output.stderr.trim()))
    }

    /// `image-setup <bundle>`: `kbf-vmm image setup` on the bundle as root, then the
    /// bundle goes back to the VM user, flagged `uchg`. Returns `kbf-vmm`'s report.
    ///
    /// # Errors
    /// Why it was refused (a path the helper will not take, no VM user, a program that is
    /// not root's) or failed. A bundle that was taken is given back to the VM user,
    /// unflagged, when `kbf-vmm` or the flag fails.
    pub fn image_setup(&self, bundle: &str) -> Result<String, String> {
        let _one = self.one_image_job();
        let image = &self.settings.image;
        let name = image.name(bundle)?;
        let role = self.vm_role()?;
        trusted_file(&image.vmm)?;
        trusted_file(&image.guest)?;
        let images = image.open()?;
        let path = image.dir.join(&name);
        let claim = Claim::take(&images, &name)?;
        tracing::info!(bundle = %name, "image-setup: running kbf-vmm on the bundle as root");
        let owner = Some((role.account.uid, role.account.gid));
        let guest = ImageSettings::text(&image.guest);
        let sealed = ImageSettings::text(&path).and_then(|base| {
            let report =
                self.run_vmm(&["image", "setup", "--base", base, "--guest-binary", guest?])?;
            claim.give_files(owner)?;
            let files: Vec<PathBuf> = claim.names().map(|file| path.join(file)).collect();
            self.flag_immutable(&files)?;
            claim.give_dir(owner)?;
            self.flag_immutable(std::slice::from_ref(&path))?;
            Ok(report)
        });
        match sealed {
            Ok(report) => {
                tracing::info!(bundle = %name, uid = role.account.uid, "image-setup done; the bundle is the VM user's again, flagged uchg");
                Ok(report)
            }
            Err(why) => {
                warn_if_failed(
                    &format!("giving {name} back after a failure"),
                    claim.give_back(owner),
                );
                tracing::warn!(bundle = %name, "image-setup failed: {why}");
                Err(why)
            }
        }
    }

    /// `image-harvest <bundle> <out>`: makes `out` (root's, mode 0700, new), then
    /// `kbf-vmm image harvest` of the bundle into `out/files`. Returns `kbf-vmm`'s
    /// report.
    ///
    /// # Errors
    /// Why it was refused (a path the helper will not take, `out` exists, a program that
    /// is not root's) or failed; an `out` made for a failed run is removed if it is
    /// empty. The bundle's owners go back to what they were.
    pub fn image_harvest(&self, bundle: &str, out: &str) -> Result<String, String> {
        let _one = self.one_image_job();
        let image = &self.settings.image;
        let name = image.name(bundle)?;
        let out_name = image.name(out)?;
        if name == out_name {
            return Err("the output directory must not be the bundle".to_owned());
        }
        trusted_file(&image.vmm)?;
        let images = image.open()?;
        make_private_dir(&images, &out_name)?;
        let result = (|| {
            let claim = Claim::take(&images, &name)?;
            tracing::info!(bundle = %name, out = %out_name, "image-harvest: running kbf-vmm on the bundle as root");
            let base = image.dir.join(&name);
            let target = image.dir.join(&out_name).join(HARVEST_FILES);
            let ran = ImageSettings::text(&base).and_then(|bundle| {
                self.run_vmm(&[
                    "image",
                    "harvest",
                    "--bundle",
                    bundle,
                    "--out",
                    ImageSettings::text(&target)?,
                ])
            });
            let back = claim.give_back(None);
            let report = ran?;
            back.map(|()| report)
        })();
        if let Err(why) = &result {
            // Removes the directory only if it is empty: a partial copy stays for a person.
            let _ = unlinkat(&images, out_name.as_str(), AtFlags::REMOVEDIR);
            tracing::warn!(bundle = %name, "image-harvest failed: {why}");
        }
        result
    }
}

/// Makes the new directory `name` in `images`: the helper's, mode 0700. An existing name
/// of any kind (a link included) is an error.
fn make_private_dir(images: &OwnedFd, name: &str) -> Result<(), String> {
    mkdirat(images, name, Mode::from_raw_mode(0o700))
        .map_err(at(&format!("making {name} in the images directory")))?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let made =
        openat(images, name, flags, Mode::empty()).map_err(at(&format!("opening {name}")))?;
    fchown(
        &made,
        Some(Uid::from_raw(rustix::process::geteuid().as_raw())),
        Some(Gid::from_raw(rustix::process::getegid().as_raw())),
    )
    .map_err(at(&format!("taking {name}")))?;
    fchmod(&made, Mode::from_raw_mode(0o700)).map_err(at(&format!("locking {name}")))
}

#[cfg(test)]
mod tests;
