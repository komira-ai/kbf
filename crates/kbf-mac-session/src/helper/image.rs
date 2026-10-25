//! The image verbs, `image-setup` and `image-harvest`: the two root-only steps of
//! building a guest image, run by the helper so the daemon (not a person with `sudo`)
//! can drive them.
//!
//! Two directories: the staging directory (`--staging-dir`), which the VM user owns and
//! `kbf-vmm image base` writes a new bundle into, and the images directory
//! (`--images-dir`), which is root's, mode 0755 at most, and holds sealed bases.
//!
//! - `image-setup <bundle>` takes the bundle named in the staging directory, moves it
//!   into the images directory with a rename (one volume, no copy), runs `kbf-vmm image
//!   setup --base <images>/<name> --guest-binary <guest>`, which attaches the disk image
//!   and writes the guest's first-boot files into its Data volume, and seals it: the base
//!   STAYS root's, the directory mode 0750 and the files 0640 with the VM user's primary
//!   group, and the files, then the directory, get the user-immutable flag (`uchg`) from
//!   root, which the VM user cannot clear. The VM user clones it through its group
//!   (`clonefile` keeps owner-independent read access; `kbf-vmm` clears `uchg` on each
//!   clone, which is the user's own).
//! - `image-harvest <bundle> <out>` runs `kbf-vmm image harvest --bundle <bundle> --out
//!   <out>/files`, which attaches the disk read-only and copies a fixed allowlist out.
//!   The bundle is named in the staging directory (a kept clone) or the images directory
//!   (a sealed base, which is already closed and is left as it is). The helper makes
//!   `<out>` in the images directory first: root's, mode 0700, new.
//!
//! What the caller cannot choose: the program (`--vmm`, an installed root-owned file),
//! the guest binary (`--guest-binary`, the same), every flag and every argument but the
//! two paths, and where those paths lead. A path is accepted only as a name directly
//! inside the staging or images directory: the helper compares components, so `..`, a
//! longer path and a lookalike sibling are refused, and then builds the path it passes
//! on from the configured directory and that one name, never from the caller's text.
//!
//! A bundle the VM user owns is taken before anything is done with it, because the user
//! could swap what root is about to attach. The helper opens the name without following
//! a link, makes the directory root's and mode 0700 through the descriptor, checks that
//! the name still leads to that directory, and makes every file in it root's, refusing
//! anything but plain single-link files (a symlink, a hard link to a file elsewhere, a
//! subdirectory). The staging directory is the user's, so the name can still be swapped
//! after that: the rename is therefore checked again by name in the images directory
//! (same device and inode as the descriptor held) and undone if it moved something else.
//! The images directory is root's and closed, so what is in it cannot be touched by the
//! user. An object that is root's already and closed to group and others is not taken
//! or changed. When a step fails, the bundle goes back to the staging directory with the
//! owners and modes it had.
//!
//! One image verb runs at a time. The ledger's lock is not held: `kbf-vmm` takes minutes.

use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{MutexGuard, PoisonError};

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Gid, Mode, OFlags, RawMode, Stat, Uid, chownat, fchmod, fchown,
    fstat, mkdirat, openat, renameat, statat, unlinkat,
};

use super::Helper;
use crate::autologin::CommandOutput;
use crate::proto::IMAGE_REPORT_BYTES;
use crate::start::owned_privately;

/// Sets the user-immutable flag on the paths it is given.
pub const CHFLAGS: &str = "/usr/bin/chflags";

/// What `kbf-vmm harvest` creates inside the helper's `<out>`.
const HARVEST_FILES: &str = "files";

/// A sealed base's modes: the directory and its files.
const SEALED_DIR: RawMode = 0o750;
const SEALED_FILE: RawMode = 0o640;

/// Where the image verbs act, and the installed programs they use.
#[derive(Clone, Debug)]
pub struct ImageSettings {
    /// The images directory: sealed bases and harvest directories are names directly in
    /// it. Root's, and closed to group and others.
    pub dir: PathBuf,
    /// The staging directory the VM user builds new bundles in and keeps clones in.
    pub staging: PathBuf,
    /// The `kbf-vmm` to run.
    pub vmm: PathBuf,
    /// The `kbf-guest` for macOS that `image-setup` has `kbf-vmm` copy into the guest.
    pub guest: PathBuf,
}

/// Which directory a bundle was named in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    Images,
    Staging,
}

impl ImageSettings {
    /// The paths a test uses: everything under `dir`.
    #[cfg(test)]
    pub(crate) fn under(dir: &Path) -> Self {
        Self {
            dir: dir.join("images"),
            staging: dir.join("staging"),
            vmm: dir.join("kbf-vmm"),
            guest: dir.join("kbf-guest"),
        }
    }

    /// The one name `path` has directly inside `root` (`label` names it in an error).
    ///
    /// # Errors
    /// The path is not under `root`, goes up (`..`), is deeper than one name, or holds NUL.
    fn name(root: &Path, label: &str, path: &str) -> Result<String, String> {
        if path.contains('\0') {
            return Err("a path holds NUL".to_owned());
        }
        let Ok(rest) = Path::new(path).strip_prefix(root) else {
            return Err(format!(
                "{path} is not inside the {label} {}",
                root.display()
            ));
        };
        let mut parts = rest.components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(name)), None) => Ok(name.to_string_lossy().into_owned()),
            (Some(Component::ParentDir), _) | (_, Some(Component::ParentDir)) => {
                Err(format!("{path} goes up with `..`"))
            }
            _ => Err(format!(
                "{path} is not a name directly inside the {label} {}",
                root.display()
            )),
        }
    }

    /// A name in the images directory.
    fn images_name(&self, path: &str) -> Result<String, String> {
        Self::name(&self.dir, "images directory", path)
    }

    /// A name in the staging directory.
    fn staging_name(&self, path: &str) -> Result<String, String> {
        Self::name(&self.staging, "staging directory", path)
    }

    /// A bundle named in either directory.
    fn home(&self, path: &str) -> Result<(Home, String), String> {
        if Path::new(path).starts_with(&self.staging) {
            return Ok((Home::Staging, self.staging_name(path)?));
        }
        if Path::new(path).starts_with(&self.dir) {
            return Ok((Home::Images, self.images_name(path)?));
        }
        Err(format!(
            "{path} is inside neither the staging directory {} nor the images directory {}",
            self.staging.display(),
            self.dir.display()
        ))
    }

    /// The directory `home` names.
    fn root(&self, home: Home) -> &Path {
        match home {
            Home::Images => &self.dir,
            Home::Staging => &self.staging,
        }
    }

    /// Opens the images directory, checked: the helper's own and closed to group and
    /// others.
    fn open_images(&self) -> Result<OwnedFd, String> {
        let here = self.dir.display();
        let dir = open_dir(&self.dir)?;
        let stat = fstat(&dir).map_err(at(&here.to_string()))?;
        closed_dir(&stat).map_err(|why| format!("the images directory {here}: {why}"))?;
        Ok(dir)
    }

    /// `path` as text for an argument.
    fn text(path: &Path) -> Result<&str, String> {
        path.to_str()
            .ok_or_else(|| format!("{} is not UTF-8", path.display()))
    }
}

/// Opens the directory `path`. The configured path may go through a link (`/var` is
/// one); the names below it are opened without following any.
fn open_dir(path: &Path) -> Result<OwnedFd, String> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    openat(CWD, path, flags, Mode::empty()).map_err(at(&format!("opening {}", path.display())))
}

/// Whether the images directory can be trusted: the helper's own, and not writable by
/// group or others.
fn closed_dir(stat: &Stat) -> Result<(), String> {
    let me = rustix::process::geteuid().as_raw();
    if stat.st_uid != me {
        return Err(format!(
            "owned by uid {}, not the helper's uid {me}",
            stat.st_uid
        ));
    }
    if stat.st_mode & 0o022 != 0 {
        return Err("writable by its group or others".to_owned());
    }
    Ok(())
}

/// A program the helper runs or reads: a regular file, the helper's own, not writable by
/// anyone else. Followed through links (the kit's `current` is one).
fn trusted_file(path: &Path) -> Result<(), String> {
    let here = path.display();
    let file = openat(CWD, path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(at(&format!("opening {here}")))?;
    let stat = fstat(&file).map_err(at(&here.to_string()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(format!("{here} is not a regular file"));
    }
    owned_privately(&stat).map_err(|why| format!("{here}: {why}"))
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

    /// Whether this is the helper's own and closed to group and others: nothing to take.
    fn closed(self) -> bool {
        closed_by(self.uid, rustix::process::geteuid().as_raw(), self.mode)
    }
}

/// Whether an object owned by `uid` with `mode` is `me`'s and closed to group and others.
fn closed_by(uid: u32, me: u32, mode: RawMode) -> bool {
    uid == me && mode & 0o022 == 0
}

/// The helper's own ids, as `fchown` takes them.
fn mine() -> (Uid, Gid) {
    (
        Uid::from_raw(rustix::process::geteuid().as_raw()),
        Gid::from_raw(rustix::process::getegid().as_raw()),
    )
}

/// Checks that `now`, the name looked up again after the directory was taken (or moved),
/// is the directory `held` was opened as and is the helper's.
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

/// `result`; when it is an error, `undo` runs first (its own failure is only logged).
fn or_undo<T>(
    what: &str,
    result: Result<T, String>,
    undo: impl FnOnce() -> Result<(), String>,
) -> Result<T, String> {
    if result.is_err() {
        warn_if_failed(what, undo());
    }
    result
}

/// A bundle directory taken for root: its descriptor, and the owners it had.
struct Claim {
    dir: OwnedFd,
    /// What `fstat` said when the descriptor was opened.
    held: Stat,
    was: Owner,
    /// Whether the directory was changed (it was not root's and closed already).
    dir_taken: bool,
    /// Every file in it, with its owner before the claim and whether it was changed, in
    /// name order.
    files: Vec<(String, Owner, bool)>,
}

impl Claim {
    /// Takes the directory `name` in `home`, never through a link.
    ///
    /// # Errors
    /// `name` is not a plain directory, changed while it was taken, or holds anything but
    /// plain single-link files. Whatever was taken is given back.
    fn take(home: &OwnedFd, name: &str) -> Result<Self, String> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let dir = openat(home, name, flags, Mode::empty()).map_err(at(&format!(
            "{name} is not a plain directory in its directory"
        )))?;
        let held = fstat(&dir).map_err(at(name))?;
        let mut claim = Self {
            dir,
            held,
            was: Owner::of(&held),
            dir_taken: false,
            files: Vec::new(),
        };
        let filled = claim.fill(home, name);
        or_undo(
            &format!("giving {name} back after a refused claim"),
            filled,
            || claim.give_back(),
        )?;
        Ok(claim)
    }

    fn fill(&mut self, home: &OwnedFd, name: &str) -> Result<(), String> {
        let (uid, gid) = mine();
        if !self.was.closed() {
            self.dir_taken = true;
            fchown(&self.dir, Some(uid), Some(gid)).map_err(at(&format!("taking {name}")))?;
            fchmod(&self.dir, Mode::from_raw_mode(0o700))
                .map_err(at(&format!("locking {name}")))?;
        }
        let now = statat(home, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(at(&format!("looking {name} up again")))?;
        check_claim(name, &self.held, &now)?;
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
            let was = Owner::of(&stat);
            let taken = !was.closed();
            if taken {
                chownat(
                    &self.dir,
                    &file,
                    Some(uid),
                    Some(gid),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(at(&format!("taking {name}/{file}")))?;
            }
            self.files.push((file, was, taken));
        }
        self.files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(())
    }

    /// Gives back what was taken, files first (the directory is root's and closed to the
    /// others until the last step): the owners and modes they had.
    fn give_back(&self) -> Result<(), String> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        for (file, was, taken) in &self.files {
            if *taken {
                let (uid, gid) = (Uid::from_raw(was.uid), Gid::from_raw(was.gid));
                let opened = openat(&self.dir, file.as_str(), flags, Mode::empty())
                    .map_err(at(&format!("opening {file}")))?;
                fchown(&opened, Some(uid), Some(gid))
                    .map_err(at(&format!("giving {file} back")))?;
                fchmod(&opened, Mode::from_raw_mode(was.mode))
                    .map_err(at(&format!("restoring the mode of {file}")))?;
            }
        }
        if self.dir_taken {
            let (uid, gid) = (Uid::from_raw(self.was.uid), Gid::from_raw(self.was.gid));
            fchown(&self.dir, Some(uid), Some(gid)).map_err(at("giving the bundle back"))?;
            fchmod(&self.dir, Mode::from_raw_mode(self.was.mode))
                .map_err(at("restoring the bundle's mode"))?;
        }
        Ok(())
    }

    /// Seals the files: root's, mode 0640, group `gid`. Each is opened without following
    /// a link (the directory is closed to everyone else, so the files are the ones taken).
    fn seal_files(&self, gid: u32) -> Result<(), String> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let (uid, _) = mine();
        for (file, _, _) in &self.files {
            let opened = openat(&self.dir, file.as_str(), flags, Mode::empty())
                .map_err(at(&format!("opening {file}")))?;
            fchown(&opened, Some(uid), Some(Gid::from_raw(gid)))
                .map_err(at(&format!("sealing {file}")))?;
            fchmod(&opened, Mode::from_raw_mode(SEALED_FILE))
                .map_err(at(&format!("sealing {file}")))?;
        }
        Ok(())
    }

    /// Seals the directory: root's, mode 0750, group `gid`.
    fn seal_dir(&self, gid: u32) -> Result<(), String> {
        let (uid, _) = mine();
        fchown(&self.dir, Some(uid), Some(Gid::from_raw(gid))).map_err(at("sealing the bundle"))?;
        fchmod(&self.dir, Mode::from_raw_mode(SEALED_DIR)).map_err(at("sealing the bundle"))
    }

    /// The names of the files.
    fn names(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|(file, _, _)| file.as_str())
    }
}

/// Moves `name` from `from` to `to` (one volume) and checks that what arrived is the
/// directory `held`; if it is something else, it is moved back.
fn move_into(from: &OwnedFd, to: &OwnedFd, name: &str, held: &Stat) -> Result<(), String> {
    renameat(from, name, to, name).map_err(at(&format!("moving {name}")))?;
    let arrived = statat(to, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(at(&format!("looking {name} up after the move")));
    let checked = arrived.and_then(|now| check_claim(name, held, &now));
    or_undo(&format!("moving {name} back"), checked, || {
        renameat(to, name, from, name).map_err(at("renaming"))
    })
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
            .map_err(|why| format!("running kbf-vmm: {why}"))?;
        if output.success {
            return Ok(report(&output.stdout));
        }
        Err(format!(
            "kbf-vmm failed: {} {}",
            report(output.stderr.trim()),
            report(output.stdout.trim()),
        ))
    }

    /// `chflags -h <flag> <paths>`.
    fn chflags(&self, flag: &str, paths: &[PathBuf]) -> Result<(), String> {
        let mut args = vec!["-h", flag];
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

    /// `image-setup <bundle>`: the bundle named in the staging directory is taken, moved
    /// into the images directory, set up by `kbf-vmm image setup` as root, and sealed
    /// there (root's, 0750/0640, group the VM user's, `uchg`). Returns `kbf-vmm`'s report.
    ///
    /// # Errors
    /// Why it was refused (a path the helper will not take, no VM user, a program that is
    /// not root's, a base of that name already) or failed. A bundle that was taken goes
    /// back to the staging directory with the owners and modes it had.
    pub fn image_setup(&self, bundle: &str) -> Result<String, String> {
        let _one = self.one_image_job();
        let image = &self.settings.image;
        let name = image.staging_name(bundle)?;
        let role = self.vm_role()?;
        trusted_file(&image.vmm)?;
        trusted_file(&image.guest)?;
        let images = image.open_images()?;
        let staging = open_dir(&image.staging)?;
        match statat(&images, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Err(rustix::io::Errno::NOENT) => {}
            _ => return Err(format!("{name} is in the images directory already")),
        }
        let claim = Claim::take(&staging, &name)?;
        tracing::info!(bundle = %name, "image-setup: moving the bundle into the images directory");
        move_into(&staging, &images, &name, &claim.held).inspect_err(|_| {
            warn_if_failed(&format!("giving {name} back"), claim.give_back());
        })?;
        let path = image.dir.join(&name);
        let gid = role.account.gid;
        let sealed = ImageSettings::text(&path).and_then(|base| {
            let guest = ImageSettings::text(&image.guest)?;
            let report =
                self.run_vmm(&["image", "setup", "--base", base, "--guest-binary", guest])?;
            claim.seal_files(gid)?;
            let files: Vec<PathBuf> = claim.names().map(|file| path.join(file)).collect();
            self.chflags("uchg", &files)?;
            claim.seal_dir(gid)?;
            self.chflags("uchg", std::slice::from_ref(&path))?;
            Ok(report)
        });
        or_undo(
            &format!("moving {name} back after a failure"),
            sealed,
            || renameat(&images, name.as_str(), &staging, name.as_str()).map_err(at("renaming")),
        )
        .inspect_err(|why| {
            warn_if_failed(&format!("giving {name} back"), claim.give_back());
            tracing::warn!(bundle = %name, "image-setup failed: {why}");
        })
        .inspect(|_| {
            tracing::info!(bundle = %name, gid, "image-setup done; the base is sealed: root's, 0750/0640, uchg");
        })
    }

    /// `image-harvest <bundle> <out>`: makes `out` in the images directory (root's, mode
    /// 0700, new), then `kbf-vmm image harvest` of the bundle into `out/files`. Returns
    /// `kbf-vmm`'s report.
    ///
    /// # Errors
    /// Why it was refused (a path the helper will not take, `out` exists, a program that
    /// is not root's) or failed; an `out` made for a failed run is removed if it is
    /// empty. The bundle's owners go back to what they were.
    pub fn image_harvest(&self, bundle: &str, out: &str) -> Result<String, String> {
        let _one = self.one_image_job();
        let image = &self.settings.image;
        let (home, name) = image.home(bundle)?;
        let out_name = image.images_name(out)?;
        if home == Home::Images && name == out_name {
            return Err("the output directory must not be the bundle".to_owned());
        }
        trusted_file(&image.vmm)?;
        let images = image.open_images()?;
        let staging;
        let from = match home {
            Home::Images => &images,
            Home::Staging => {
                staging = open_dir(&image.staging)?;
                &staging
            }
        };
        make_private_dir(&images, &out_name)?;
        let result = (|| {
            let claim = Claim::take(from, &name)?;
            tracing::info!(bundle = %name, out = %out_name, "image-harvest: running kbf-vmm on the bundle as root");
            let base = image.root(home).join(&name);
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
            let back = claim.give_back();
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
    let (uid, gid) = mine();
    fchown(&made, Some(uid), Some(gid)).map_err(at(&format!("taking {name}")))?;
    fchmod(&made, Mode::from_raw_mode(0o700)).map_err(at(&format!("locking {name}")))
}

#[cfg(test)]
mod tests;
