//! The shared open of [`FsStore`] (see the module docs of `fs_store`): no `LOCK`, a
//! locked file of its own under `instances/`, and temporary files named after it in
//! `tmp-shared/`, a directory an exclusive open never touches.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;

use super::*;

/// The sequence number in an instance name, so two stores opened in one process in the
/// same microsecond differ.
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);

impl FsStore {
    /// Opens the store in `dir` for writers whose keys never collide and that never
    /// delete (see the module docs, "Shared mode"): the directory may be open in other
    /// shared stores, in this process or another, and in an exclusive store. Creates the
    /// directory and its layout if missing, and removes what stopped shared stores left
    /// in `tmp-shared/`. A write is refused while the file system would be left with less than
    /// `floor` free.
    ///
    /// # Errors
    /// The directory cannot be created or read, or the store's instance file cannot be
    /// created or locked.
    pub fn open_shared(dir: &Path, floor: Floor) -> Result<Self, FsStoreError> {
        Self::open_shared_with_space(dir, floor, statvfs_source())
    }

    /// [`FsStore::open_shared`], with the file system's size and free space read from
    /// `space` instead of `statvfs`: for tests that need a file system of a given size.
    ///
    /// # Errors
    /// As [`FsStore::open_shared`].
    #[doc(hidden)]
    pub fn open_shared_with_space(
        dir: &Path,
        floor: Floor,
        space: SpaceSource,
    ) -> Result<Self, FsStoreError> {
        fs::create_dir_all(dir).map_err(open_failed("create", dir))?;
        let (objects, legacy_tmp) = create_layout(dir)?;
        let tmp = dir.join("tmp-shared");
        fs::create_dir_all(&tmp).map_err(open_failed("create", &tmp))?;
        let instances = dir.join("instances");
        fs::create_dir_all(&instances).map_err(open_failed("create", &instances))?;
        // `tmp-shared/` is listed before `instances/`: an instance creates its file before any
        // temporary file, so a name seen in the first listing and absent from the second
        // belongs to an instance that is gone.
        let seen = entries(&tmp).map_err(open_failed("list", &tmp))?;
        let (name, path, file) = claim(&instances, instance_name, |_| {})?;
        // From here every way out of the open, an error included, unlocks.
        let lock = DirLock(file);
        sweep(dir, &legacy_tmp, &instances, &seen)?;
        for path in [
            tmp.as_path(),
            legacy_tmp.as_path(),
            instances.as_path(),
            objects.as_path(),
            dir,
        ] {
            sync_dir(path).map_err(open_failed("sync", path))?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                objects,
                floor,
                space,
                full: Notify::new(),
                tmp,
                tmp_prefix: format!("{name}."),
                next: AtomicU64::new(0),
                listing: Mutex::new(None),
                window: LISTING_WINDOW,
                walks: AtomicU64::new(0),
                lock,
                instance: Some(path),
                released: Arc::new(Gate::default()),
                dir: dir.to_path_buf(),
            }),
        })
    }
}

/// A name no other shared store of this directory has: the process id, the start time in
/// microseconds, and a sequence number. It holds no dot, which ends it in a temporary
/// file's name.
fn instance_name() -> String {
    let micros = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    let seq = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{micros}-{seq}", std::process::id())
}

/// Creates `instances/<name>` for a name from `name`, and locks it. `locked` is called
/// with the path once the lock is held. A store that finds its file no longer named by
/// its path (a shared open that was sweeping took the lock before this one and removed
/// the file) starts again with a new name.
fn claim(
    instances: &Path,
    mut name: impl FnMut() -> String,
    mut locked: impl FnMut(&Path),
) -> Result<(String, PathBuf, File), FsStoreError> {
    loop {
        let name = name();
        let path = instances.join(&name);
        let file = File::options()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(open_failed("create", &path))?;
        // Waits for a sweep that holds the new file for a moment.
        file.lock().map_err(open_failed("lock", &path))?;
        locked(&path);
        if names_file(&file, &path) {
            return Ok((name, path, file));
        }
    }
}

/// Whether `path` names the file `file` is open on.
fn names_file(file: &File, path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|named| {
        file.metadata()
            .is_ok_and(|open| (open.dev(), open.ino()) == (named.dev(), named.ino()))
    })
}

/// Removes what stopped instances left. `seen` is `tmp-shared/` as listed before this
/// store's instance file was created; `legacy_tmp` is `tmp/`, where only exclusive mode
/// writes. A removal that fails leaves its file to the next open.
fn sweep(
    dir: &Path,
    legacy_tmp: &Path,
    instances: &Path,
    seen: &[PathBuf],
) -> Result<(), FsStoreError> {
    let mut named = BTreeSet::new();
    let mut gone = BTreeSet::new();
    for path in entries(instances).map_err(open_failed("list", instances))? {
        let name = file_name(&path);
        // A file whose lock is taken here belongs to a stopped process; one whose lock
        // is held belongs to a running one (this store's own included), which is left
        // alone.
        let held = File::open(&path)
            .ok()
            .and_then(|f| f.try_lock().ok().map(|()| f));
        if held.is_some() {
            let _ = fs::remove_file(&path);
            gone.insert(name.clone());
        }
        named.insert(name);
    }
    for path in seen {
        let stopped = file_name(path)
            .split_once('.')
            .is_none_or(|(owner, _)| gone.contains(owner) || !named.contains(owner));
        if stopped {
            let _ = fs::remove_file(path);
        }
    }
    // The temporary files of exclusive mode are removed only while no exclusive store is
    // open, and none can open while this holds the lock.
    let lock_path = dir.join("LOCK");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(open_failed("open", &lock_path))?;
    let lock = DirLock(file);
    if lock.0.try_lock().is_ok() {
        for path in entries(legacy_tmp).map_err(open_failed("list", legacy_tmp))? {
            let _ = remove_leftover(&path);
        }
    }
    Ok(())
}

/// The last component of `path`.
fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// The `objects/<hh>` directories and `tmp/` of `dir`, created if missing: the layout
/// both opens share (a shared store adds `tmp-shared/` and `instances/`). Returns `objects/` and `tmp/`.
pub(super) fn create_layout(dir: &Path) -> Result<(PathBuf, PathBuf), FsStoreError> {
    let objects = dir.join("objects");
    for shard in 0..=u8::MAX {
        let path = objects.join(format!("{shard:02x}"));
        fs::create_dir_all(&path).map_err(open_failed("create", &path))?;
    }
    let tmp = dir.join("tmp");
    fs::create_dir_all(&tmp).map_err(open_failed("create", &tmp))?;
    Ok((objects, tmp))
}

#[cfg(test)]
#[path = "fs_store_shared_tests.rs"]
mod tests;
