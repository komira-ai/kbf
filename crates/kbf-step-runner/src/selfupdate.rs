//! The updater's own update (docs/design/host-updates.md, 3.5, "Updating the updater itself").
//!
//! The updater is part of the kbf release and is updated by the kbf daemon's hook as any
//! service, but never inside the trial it guards. After the update of the release has finished
//! and `verify` has succeeded, a follow-on step ([`follow_on`]) stages the new updater binary
//! beside the old, points `current` at it and tells the caller to reply and exit cleanly; the
//! unit's `Restart=always` starts the new binary. If the new binary cannot start, systemd gives
//! up after `StartLimitBurst` starts, the unit fails, and its `OnFailure=` unit runs the
//! *previous* binary's `self-update-fallback` verb, which calls [`Layout::flip_back`]: `current`
//! points at the previous binary again and the unit is started.
//!
//! Layout of the updater's binary directory:
//!
//! ```text
//! <dir>/versions/<version>    one executable per staged version
//! <dir>/versions/.<version>.stage   a version being staged (no version starts with `.`)
//! <dir>/current  -> versions/<version>   what the unit runs
//! <dir>/previous -> versions/<version>   what it ran before the last switch
//! ```
//!
//! Every move is a rename of a link made beside the old one, then a directory fsync, so a crash
//! leaves either link whole.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use update_hook::Verdict;

use crate::artifact::{ArtifactError, Staged};
use crate::runner::Finished;

/// The failed starts after which the fallback runs.
pub const START_LIMIT_BURST: u32 = 3;

/// The updater's binary directory.
///
/// **The caller gives a directory that is root-owned and closed to group and others, in
/// root-owned parents**: this type does not check it, and the fallback unit runs what `previous`
/// points at as root.
#[derive(Clone, Debug)]
pub struct Layout {
    dir: PathBuf,
}

/// What the caller does after [`follow_on`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowOn {
    /// The new binary is staged and `current` points at it: reply, then exit with status 0.
    Restart,
    /// This updater already is that version, or the update did not succeed: carry on.
    NotNeeded,
}

impl Layout {
    pub fn new(dir: &Path) -> Layout {
        Layout {
            dir: dir.to_path_buf(),
        }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The link the unit runs.
    pub fn current_path(&self) -> PathBuf {
        self.dir.join("current")
    }

    /// The link to the binary run before the last switch.
    pub fn previous_path(&self) -> PathBuf {
        self.dir.join("previous")
    }

    fn version_path(&self, version: &str) -> PathBuf {
        self.dir.join("versions").join(version)
    }

    /// The version `current` points at, `None` before the first switch.
    pub fn current(&self) -> io::Result<Option<String>> {
        link_version(&self.current_path())
    }

    /// The version `previous` points at.
    pub fn previous(&self) -> io::Result<Option<String>> {
        link_version(&self.previous_path())
    }

    /// Puts `binary` beside the others as version `version`, mode 0755, durably. Changes no link.
    ///
    /// The version `current` or `previous` points at is refused
    /// ([`io::ErrorKind::AlreadyExists`]) and left as it is: a crash between staging over it and
    /// the switch would leave the unit, or its fallback, running a binary that was never the one
    /// it switched to. The copy is made under a name no version can have (`.<version>.stage`),
    /// created afresh, so that staging one version never writes over the file of another.
    pub fn stage(&self, version: &str, binary: &Path) -> io::Result<()> {
        check_version(version)?;
        if self.in_place(version)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the version `current` or `previous` points at is not staged over",
            ));
        }
        let versions = self.dir.join("versions");
        fs::create_dir_all(&versions)?;
        let tmp = versions.join(format!(".{version}.stage"));
        // What an earlier staging of this version left when it crashed; a directory there is
        // not one, and is an error.
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&tmp)?;
        io::copy(&mut File::open(binary)?, &mut out)?;
        out.set_permissions(Permissions::from_mode(0o755))?;
        out.sync_all()?;
        fs::rename(&tmp, self.version_path(version))?;
        File::open(&versions)?.sync_all()
    }

    /// Points `current` at the staged `version` and `previous` at what `current` was. Running it
    /// again changes nothing. The version must be an executable file ([`Layout::check_binary`]).
    pub fn switch(&self, version: &str) -> io::Result<()> {
        self.check_binary(version)?;
        let before = self.current()?;
        if before.as_deref() == Some(version) {
            return Ok(());
        }
        if let Some(before) = before {
            self.set_link("previous", &before)?;
        }
        self.set_link("current", version)
    }

    /// Points `current` at `previous` again. `false` when there is no `previous` or `current`
    /// already points at it; an error, with `current` unchanged, when what `previous` names is
    /// not an executable file ([`Layout::check_binary`]), whichever `current` points at: the
    /// fallback reports a fallback it cannot run.
    pub fn flip_back(&self) -> io::Result<bool> {
        let Some(previous) = self.previous()? else {
            return Ok(false);
        };
        self.check_binary(&previous)?;
        if self.current()?.as_deref() == Some(previous.as_str()) {
            return Ok(false);
        }
        self.set_link("current", &previous)?;
        Ok(true)
    }

    /// `versions/<version>` is a regular file with an execute bit, not a link (dangling or not)
    /// and not a directory: what staging makes. [`io::ErrorKind::NotFound`] when there is
    /// nothing, [`io::ErrorKind::InvalidData`] when it is something else.
    pub fn check_binary(&self, version: &str) -> io::Result<()> {
        check_version(version)?;
        let meta = fs::symlink_metadata(self.version_path(version))?;
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("versions/{version} is not an executable file"),
            ));
        }
        Ok(())
    }

    /// Whether `current` or `previous` points at `version`.
    fn in_place(&self, version: &str) -> io::Result<bool> {
        let named = [self.current()?, self.previous()?];
        Ok(named.iter().any(|v| v.as_deref() == Some(version)))
    }

    fn set_link(&self, name: &str, version: &str) -> io::Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        symlink(Path::new("versions").join(version), &tmp)?;
        fs::rename(&tmp, self.dir.join(name))?;
        File::open(&self.dir)?.sync_all()
    }
}

fn link_version(link: &Path) -> io::Result<Option<String>> {
    match fs::read_link(link) {
        Ok(target) => Ok(target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// A version names a file: ASCII letters, digits, `.`, `-`, `_` and `+`, not starting with `.`.
fn check_version(version: &str) -> io::Result<()> {
    let plain = version
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'));
    if version.is_empty() || version.starts_with('.') || !plain {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a version that is not a plain file name",
        ));
    }
    Ok(())
}

/// The follow-on step of an update that finished: when it succeeded and this updater (version
/// `running`) is not `version`, stages the updater binary of the verified release `release` as
/// `version` and switches to it.
///
/// `binary` is the binary's path inside the release, relative and of plain names; it is taken
/// only through [`Staged::check`], so a release that changed since it was sealed, or a path that
/// leaves it, stages nothing and moves no link. There is no way to give this a file of no
/// verified release.
///
/// A version `current` or `previous` already names (a repeat after a crash between the switch
/// and the restart, or a return to the binary before) is not staged again: it is switched to
/// when its file holds the bytes of the verified binary, and refused, with nothing changed,
/// when it does not.
///
/// The record of the update is finished before this is called ([`crate::StepRunner::run`]
/// returned), so the restart interrupts nothing. It is safe to call again after a crash, and
/// the new binary, started, finds `running == version` and does nothing.
pub fn follow_on(
    finished: &Finished,
    layout: &Layout,
    running: &str,
    version: &str,
    release: &Staged,
    binary: &Path,
) -> Result<FollowOn, ArtifactError> {
    if !matches!(finished.verdict, Verdict::Succeeded { .. }) || running == version {
        return Ok(FollowOn::NotNeeded);
    }
    let new_binary = release.check(binary)?;
    if layout.in_place(version)? {
        let same = fs::read(layout.version_path(version))? == fs::read(&new_binary)?;
        if !same {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the version in place holds another binary than the verified release",
            )
            .into());
        }
    } else {
        layout.stage(version, &new_binary)?;
    }
    layout.switch(version)?;
    Ok(FollowOn::Restart)
}

/// The drop-in for the updater's own unit: restart when it exits, and run the fallback unit
/// once it has failed `burst` starts.
pub fn main_dropin(fallback_unit: &str, burst: u32) -> String {
    format!(
        "[Unit]\nOnFailure={fallback_unit}\nStartLimitIntervalSec=300\nStartLimitBurst={burst}\n\n[Service]\nRestart=always\nRestartSec=1\n"
    )
}

/// The fallback unit: the previous binary flips `current` back and the main unit is started.
pub fn fallback_unit(layout: &Layout, main_unit: &str) -> String {
    let dir = layout.dir().display();
    format!(
        "[Unit]\nDescription=Put the previous updater binary back after the new one failed to start\n\n[Service]\nType=oneshot\nExecStart={dir}/previous self-update-fallback --dir {dir}\nExecStartPost=/usr/bin/systemctl reset-failed {main_unit}\nExecStartPost=/usr/bin/systemctl start --no-block {main_unit}\n"
    )
}
