//! The new tree, held open by descriptor: the installer's releases, its scratch, and the
//! small files the migration writes in its state directory. Everything is opened
//! relative to a directory held, never by a path that could be swapped meanwhile.

use std::fs::{self, DirBuilder};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

use kbf_host::path::HostPath;
use rustix::fs::{AtFlags, Mode};
use rustix::io::Errno;

use super::error::MigrateError;
use super::roots::Roots;
use super::schema::{Journal, NewState, NewStatus};
use super::tree::{self, Rules};
use crate::CommitId;
use crate::layout::Layout;

const JOURNAL: &str = "migration.json";
const FILE_CAP: u64 = 1 << 20;

/// The state directory: `state.json`, `status.json`, `migration.json`.
pub struct StateDir {
    rules: Rules,
    fd: OwnedFd,
}

/// The new tree: its state directory, the releases and the scratch.
pub struct NewTree {
    rules: Rules,
    pub state: StateDir,
    releases: OwnedFd,
    work: OwnedFd,
}

/// Open the directory at `path` of the layout, making it (and its parents, 0755) when
/// asked; `None` when it is not there and not asked for.
fn dir(
    layout: &Layout,
    rules: &Rules,
    path: &HostPath,
    create: bool,
) -> Result<Option<OwnedFd>, MigrateError> {
    let real = layout.join(path);
    let what = || path.to_string();
    if create {
        let existed = real.exists();
        DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&real)
            .map_err(MigrateError::io(what()))?;
        if !existed {
            // Whatever the umask said: a directory made here is 0755, an existing one is judged as it is.
            fs::set_permissions(&real, fs::Permissions::from_mode(0o755))
                .map_err(MigrateError::io(what()))?;
        }
    }
    // A directory just made is there; one that is not is a directory of nothing.
    let Some(fd) = tree::absent(tree::open_root(&real)).map_err(MigrateError::io(what()))? else {
        return Ok(None);
    };
    tree::expect_dir(rules, &fd, &what())?;
    Ok(Some(fd))
}

/// The directory `name` of `parent`, made (0755) when it is not there.
fn sub(rules: &Rules, parent: &OwnedFd, name: &str) -> Result<OwnedFd, MigrateError> {
    let made = rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o755));
    let made = tree::except(made, Errno::EXIST).map_err(MigrateError::io(name))?;
    let fd = tree::open_dir(parent, name).map_err(MigrateError::io(name))?;
    if made {
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o755)).map_err(MigrateError::io(name))?;
    }
    tree::expect_dir(rules, &fd, name)?;
    Ok(fd)
}

impl StateDir {
    /// The state directory of the layout; `None` when it is not there. Nothing is made.
    pub fn open(
        layout: &Layout,
        roots: &Roots,
        rules: &Rules,
    ) -> Result<Option<StateDir>, MigrateError> {
        Ok(dir(layout, rules, &roots.new_state, false)?.map(|fd| StateDir { rules: *rules, fd }))
    }

    fn read(&self, name: &'static str) -> Result<Option<Vec<u8>>, MigrateError> {
        Ok(tree::read_file(&self.rules, &self.fd, name, FILE_CAP)?)
    }

    fn corrupt(file: &'static str) -> impl FnOnce(String) -> MigrateError {
        move |why| MigrateError::Corrupt { file, why }
    }

    pub fn journal(&self) -> Result<Option<Journal>, MigrateError> {
        self.read(JOURNAL)?
            .map(|b| Journal::parse(&b).map_err(Self::corrupt(JOURNAL)))
            .transpose()
    }

    pub fn put_journal(&self, journal: &Journal) -> Result<(), MigrateError> {
        Ok(tree::replace_file(
            &self.fd,
            JOURNAL,
            0o600,
            &journal.render(),
        )?)
    }

    pub fn state(&self) -> Result<Option<NewState>, MigrateError> {
        self.read("state.json")?
            .map(|b| NewState::parse(&b).map_err(Self::corrupt("state.json")))
            .transpose()
    }

    pub fn put_state(&self, state: &NewState) -> Result<(), MigrateError> {
        Ok(tree::replace_file(
            &self.fd,
            "state.json",
            0o600,
            &state.render(),
        )?)
    }

    pub fn put_status(&self, status: &NewStatus) -> Result<(), MigrateError> {
        Ok(tree::replace_file(
            &self.fd,
            "status.json",
            0o644,
            &status.render(),
        )?)
    }

    /// The directory the first originals of the units are kept in (0700, root's).
    pub fn unit_backup(&self) -> Result<OwnedFd, MigrateError> {
        let name = "migration-units";
        let made = rustix::fs::mkdirat(&self.fd, name, Mode::from_raw_mode(0o700));
        tree::except(made, Errno::EXIST).map_err(MigrateError::io(name))?;
        let fd = tree::open_dir(&self.fd, name).map_err(MigrateError::io(name))?;
        tree::expect_dir(&self.rules, &fd, name)?;
        Ok(fd)
    }
}

impl NewTree {
    /// The new tree of the layout, made (0755) where it is not there.
    pub fn create(layout: &Layout, roots: &Roots, rules: &Rules) -> Result<NewTree, MigrateError> {
        let state = dir(layout, rules, &roots.new_state, true)?.expect("made");
        let tree = dir(layout, rules, &roots.new_tree, true)?.expect("made");
        Ok(NewTree {
            rules: *rules,
            state: StateDir {
                rules: *rules,
                fd: state,
            },
            releases: sub(rules, &tree, "releases")?,
            work: sub(rules, &tree, "work")?,
        })
    }

    fn release(&self, commit: &CommitId) -> Result<Option<OwnedFd>, MigrateError> {
        tree::absent(tree::open_dir(&self.releases, commit.as_str()))
            .map_err(MigrateError::io(commit.as_str()))
    }

    /// Whether `releases/<commit>` is there and carries `VERIFIED`.
    pub fn verified(&self, commit: &CommitId) -> Result<bool, MigrateError> {
        let Some(dir) = self.release(commit)? else {
            return Ok(false);
        };
        Ok(tree::read_file(&self.rules, &dir, "VERIFIED", FILE_CAP)?.is_some())
    }

    /// The digest of `releases/<commit>`; `None` when it is not there.
    pub fn digest(&self, commit: &CommitId) -> Result<Option<[u8; 32]>, MigrateError> {
        let Some(dir) = self.release(commit)? else {
            return Ok(None);
        };
        Ok(Some(tree::digest(&self.rules, &dir)?))
    }

    /// Remove `releases/<commit>`.
    pub fn remove_release(&self, commit: &CommitId) -> Result<(), MigrateError> {
        tree::remove_tree(&self.releases, commit.as_str())
            .map_err(MigrateError::io(commit.as_str()))
    }

    /// Remove what an interrupted copy left in `work/`.
    pub fn sweep_work(&self) -> Result<(), MigrateError> {
        let entries = rustix::fs::Dir::read_from(&self.work).map_err(MigrateError::io("work"))?;
        for item in entries {
            let name = item
                .map_err(MigrateError::io("work"))?
                .file_name()
                .to_owned();
            if name.to_bytes().starts_with(b"adopt-") {
                let name = name.to_string_lossy().into_owned();
                tree::remove_tree(&self.work, &name).map_err(MigrateError::io(name))?;
            }
        }
        Ok(())
    }

    /// Copy the tree `src` into `releases/<commit>`: into a scratch directory first, read
    /// synced, then renamed into place. The directory
    /// `commit` must not exist.
    pub fn copy_in(&self, commit: &CommitId, src: &OwnedFd) -> Result<(), MigrateError> {
        let partial = format!("adopt-{commit}.partial");
        rustix::fs::mkdirat(&self.work, &partial, Mode::from_raw_mode(0o755))
            .map_err(MigrateError::io(partial.as_str()))?;
        let result = self.fill(&partial, src);
        if result.is_err() {
            let _ = tree::remove_tree(&self.work, &partial);
            return result;
        }
        rustix::fs::renameat(&self.work, &partial, &self.releases, commit.as_str())
            .map_err(MigrateError::io(commit.as_str()))?;
        rustix::fs::fsync(&self.releases).map_err(MigrateError::io("releases"))
    }

    fn fill(&self, partial: &str, src: &OwnedFd) -> Result<(), MigrateError> {
        let dst = tree::open_dir(&self.work, partial).map_err(MigrateError::io(partial))?;
        rustix::fs::fchmod(&dst, Mode::from_raw_mode(0o755)).map_err(MigrateError::io(partial))?;
        tree::copy(&self.rules, src, &dst)?;
        rustix::fs::fsync(&dst).map_err(MigrateError::io(partial))
    }

    /// What `releases/<name>` is (a link's target as written), or `None`.
    pub fn link_target(&self, name: &str) -> Result<Option<String>, MigrateError> {
        let read = tree::absent(rustix::fs::readlinkat(&self.releases, name, Vec::new()));
        let target = read.map_err(MigrateError::io(name))?;
        Ok(target.map(|t| t.to_string_lossy().into_owned()))
    }

    /// Make `releases/<name>` point at `target` (the sibling directory of that name), or
    /// not be there. A new link is made beside the old and renamed over it, so the name
    /// is the old target or the new one at every moment. Whether anything changed.
    pub fn set_link(&self, name: &str, target: Option<&CommitId>) -> Result<bool, MigrateError> {
        let have = self.link_target(name)?;
        let Some(target) = target else {
            if have.is_none() {
                return Ok(false);
            }
            rustix::fs::unlinkat(&self.releases, name, AtFlags::empty())
                .map_err(MigrateError::io(name))?;
            return Ok(true);
        };
        if have.as_deref() == Some(target.as_str()) {
            return Ok(false);
        }
        let temp = format!("{name}.new");
        let old = rustix::fs::unlinkat(&self.releases, &temp, AtFlags::empty());
        tree::except(old, Errno::NOENT).map_err(MigrateError::io(temp.as_str()))?;
        rustix::fs::symlinkat(target.as_str(), &self.releases, &temp)
            .map_err(MigrateError::io(temp.as_str()))?;
        rustix::fs::renameat(&self.releases, &temp, &self.releases, name)
            .map_err(MigrateError::io(name))?;
        rustix::fs::fsync(&self.releases).map_err(MigrateError::io("releases"))?;
        Ok(true)
    }
}
