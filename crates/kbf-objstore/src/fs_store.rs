//! An [`ObjectStore`] in a directory on the local disk: the store of a single-host farm
//! that keeps its blobs across a restart without an S3 service (`kbf-server
//! --cold-store=file:///dir`).
//!
//! Layout under the directory:
//!
//! - `LOCK`: locked exclusively while a store is open on the directory, so a second
//!   open (another process, or this one) is refused rather than sharing it.
//! - `objects/<hh>/<h>`: one file per object. `<h>` is the SHA-256 of the key in 64
//!   lowercase hex digits and `<hh>` its first two, so every file name has the same
//!   length whatever the key (a key may be 1024 bytes, a file name 255) and the files
//!   spread over 256 directories, all created at open. A file is a header (`kbf-obj1`,
//!   the key's length as a big-endian `u16`, the key) and then the object's bytes.
//! - `tmp/`: objects being written. Emptied at open: a file there is a write that a
//!   crash or a failure cut short, and that was never answered.
//!
//! A write goes to a new file in `tmp/`, which is synced, then linked under its final
//! name, then unlinked from `tmp/`; then the final name's directory is synced, and only
//! then is the write answered. A link never replaces an existing name, so the store
//! has conditional writes: of two writers of one key, exactly one succeeds and the
//! other is answered [`ObjectStoreError::AlreadyExists`]. A crash never leaves a partial
//! object under its final name, and an answered write survives a power cut.
//!
//! A read checks that the file's header names the key asked for, and cuts the range at
//! the object's end as S3 does. A file too short to hold its header (a final name
//! whose bytes never reached the disk, which this write order does not produce but a
//! file system may after a crash) is read as no object at all: `NotFound`, so kbf
//! marks it missing and the blob is uploaded again; a listing skips it. The store does
//! not hash what it returns: kbf checks every blob it reads against its digest, from
//! any store. A listing opens every object file to read its key (`list` serves rebuild
//! and the sweep only). Nothing reads the objects at open: on a large directory that
//! would hold up every start, so a lost or damaged object is found when it is read, or
//! by [`ObjectStore::may_hold`] before an action-cache hit is served.
//!
//! A write is refused when the file system has less free space than the store's floor
//! plus the object's length, so a full disk refuses uploads instead of failing part-way
//! through them (and the Raft log, often on the same disk, keeps room). Nothing is ever
//! evicted: the directory only grows.
//!
//! No Object Lock. I/O runs on tokio's blocking pool, so every call needs a tokio
//! runtime. An I/O failure is [`ObjectStoreError::Transport`], naming the operation
//! and the path.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectInfo, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

/// The first bytes of every object file.
const MAGIC: &[u8; 8] = b"kbf-obj1";

/// The longest header: the magic, the key's length, and a key of 1024 bytes.
const MAX_HEADER: usize = MAGIC.len() + 2 + 1024;

/// Why a store could not be opened on a directory.
#[derive(Debug, thiserror::Error)]
pub enum FsStoreError {
    /// A file system call failed.
    #[error("{op} {path}: {source}")]
    Io {
        /// What the store was doing.
        op: &'static str,
        /// The path it was doing it to.
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The directory's `LOCK` is held: another store is open on it.
    #[error("lock {path}: {source} (is another server using this directory?)")]
    Locked {
        /// The lock file.
        path: PathBuf,
        #[source]
        source: std::fs::TryLockError,
    },
}

/// A bucket in a local directory (see the module docs). Conditional writes, no Object
/// Lock.
pub struct FsStore {
    inner: Arc<Inner>,
}

struct Inner {
    objects: PathBuf,
    /// The free bytes a write must leave on the file system.
    min_free: u64,
    tmp: PathBuf,
    /// The name of the next temporary file.
    next: AtomicU64,
    /// Held for the store's life: closing it releases the lock.
    _lock: File,
}

impl FsStore {
    /// Opens the store in `dir`, creating the directory and its layout if missing,
    /// locking it, and removing every file left in `tmp/`. A write is refused while the
    /// file system would be left with less than `min_free` bytes free.
    ///
    /// # Errors
    /// The directory cannot be created, locked or read, or a leftover cannot be removed.
    pub fn open(dir: &Path, min_free: u64) -> Result<Self, FsStoreError> {
        fs::create_dir_all(dir).map_err(open_failed("create", dir))?;
        let lock_path = dir.join("LOCK");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(open_failed("open", &lock_path))?;
        lock.try_lock().map_err(|source| FsStoreError::Locked {
            path: lock_path.clone(),
            source,
        })?;
        let objects = dir.join("objects");
        for shard in 0..=u8::MAX {
            let path = objects.join(format!("{shard:02x}"));
            fs::create_dir_all(&path).map_err(open_failed("create", &path))?;
        }
        let tmp = dir.join("tmp");
        fs::create_dir_all(&tmp).map_err(open_failed("create", &tmp))?;
        for path in entries(&tmp).map_err(open_failed("list", &tmp))? {
            fs::remove_file(&path).map_err(open_failed("remove", &path))?;
        }
        for path in [tmp.as_path(), objects.as_path(), dir] {
            sync_dir(path).map_err(open_failed("sync", path))?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                objects,
                min_free,
                tmp,
                next: AtomicU64::new(0),
                _lock: lock,
            }),
        })
    }
}

impl Inner {
    /// The directory and the file of `key`'s object.
    fn place(&self, key: &ObjectKey) -> (PathBuf, PathBuf) {
        let name = hex::encode(Sha256::digest(key.as_str().as_bytes()));
        let dir = self.objects.join(&name[..2]);
        let file = dir.join(name);
        (dir, file)
    }

    /// Writes `body` as `key`'s object (see the module docs). The temporary file is
    /// removed whether or not the object was linked.
    fn put(&self, key: &ObjectKey, body: &[u8]) -> Result<(), ObjectStoreError> {
        self.room_for(body.len() as u64)?;
        let (dir, path) = self.place(key);
        let tmp = self
            .tmp
            .join(self.next.fetch_add(1, Ordering::Relaxed).to_string());
        let linked = write_new(&tmp, key, body).and_then(|()| link(&tmp, &path, key));
        let removed = fs::remove_file(&tmp);
        linked?;
        removed.map_err(failed("remove", &tmp))?;
        sync_dir(&dir).map_err(failed("sync", &dir))
    }

    /// Refuses a write of `len` bytes that would leave less than the floor free.
    fn room_for(&self, len: u64) -> Result<(), ObjectStoreError> {
        let stat = rustix::fs::statvfs(&self.objects)
            .map_err(io::Error::from)
            .map_err(failed("statvfs", &self.objects))?;
        let free = stat.f_bavail.saturating_mul(stat.f_frsize);
        if free < self.min_free.saturating_add(len) {
            return Err(ObjectStoreError::Transport(Arc::new(Full {
                dir: self.objects.clone(),
                free,
                floor: self.min_free,
                len,
            })));
        }
        Ok(())
    }

    fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        let (_, path) = self.place(key);
        Ok(Object::open(&path)?.is_some_and(|o| o.key == *key))
    }

    fn get(&self, key: &ObjectKey, range: ByteRange) -> Result<Bytes, ObjectStoreError> {
        let (_, path) = self.place(key);
        let Some(object) = Object::open(&path)? else {
            return Err(ObjectStoreError::NotFound(key.clone()));
        };
        if object.key != *key {
            return Err(ObjectStoreError::Protocol(format!(
                "{} holds object {}, not {key}",
                path.display(),
                object.key
            )));
        }
        if range.offset() >= object.size {
            return Err(ObjectStoreError::InvalidRange {
                key: key.clone(),
                range,
            });
        }
        // At most the object's size, which is at most the file's.
        let len = (object.size.min(range.offset() + range.size()) - range.offset()) as usize;
        let mut bytes = vec![0; len];
        object
            .file
            .read_exact_at(&mut bytes, object.header + range.offset())
            .map_err(failed("read", &path))?;
        Ok(Bytes::from(bytes))
    }

    fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let (_, path) = self.place(key);
        match fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            removed => removed.map_err(failed("remove", &path)),
        }
    }

    /// Reads the key of every object, keeps those under `prefix` after `after`, and
    /// returns the first `max` in key order.
    fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&str>,
        max: usize,
    ) -> Result<ListPage, ObjectStoreError> {
        let mut found = BTreeMap::new();
        for shard in entries(&self.objects).map_err(failed("list", &self.objects))? {
            for path in entries(&shard).map_err(failed("list", &shard))? {
                let object = Object::open(&path)?;
                found.extend(
                    object
                        .map(|o| (o.key, o.size))
                        .filter(|(k, _)| prefix.matches(k) && after.is_none_or(|a| k.as_str() > a)),
                );
            }
        }
        let mut rest = found.into_iter();
        let objects: Vec<ObjectInfo> = rest
            .by_ref()
            .take(max)
            .map(|(key, size)| ObjectInfo { key, size })
            .collect();
        // The token is the last key of this page; the next page starts after it.
        let next = rest
            .next()
            .and(objects.last())
            .map(|last| ListToken(last.key.as_str().to_owned()));
        Ok(ListPage { objects, next })
    }
}

/// An open object file and what its header says.
struct Object {
    file: File,
    key: ObjectKey,
    /// The header's length: where the object's bytes start.
    header: u64,
    /// The object's length.
    size: u64,
}

impl Object {
    /// Opens the object file at `path`: `None` if there is none, or if it is too short
    /// to hold a header or does not start with one.
    ///
    /// # Errors
    /// The file cannot be opened or read.
    fn open(path: &Path) -> Result<Option<Self>, ObjectStoreError> {
        let file = match File::open(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            opened => opened.map_err(failed("open", path))?,
        };
        let len = file.metadata().map_err(failed("stat", path))?.len();
        let mut head = [0; MAX_HEADER];
        let read = file.read_at(&mut head, 0).map_err(failed("read", path))?;
        Ok(parse_header(&head[..read]).map(|(key, header)| Self {
            file,
            key,
            header,
            // The header was read from the file, so it is no longer than it.
            size: len - header,
        }))
    }
}

/// The header of `key`'s object file.
fn header(key: &ObjectKey) -> Vec<u8> {
    let key = key.as_str().as_bytes();
    // A key is at most 1024 bytes.
    let len = (key.len() as u16).to_be_bytes();
    [MAGIC.as_slice(), &len, key].concat()
}

/// The key and the header's length, if `bytes` start with a header.
fn parse_header(bytes: &[u8]) -> Option<(ObjectKey, u64)> {
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let (len, rest) = rest.split_first_chunk::<2>()?;
    let len = usize::from(u16::from_be_bytes(*len));
    let key = ObjectKey::new(std::str::from_utf8(rest.get(..len)?).ok()?).ok()?;
    Some((key, (MAGIC.len() + 2 + len) as u64))
}

/// Writes `key`'s header and `body` to a new file at `tmp`, and syncs it.
fn write_new(tmp: &Path, key: &ObjectKey, body: &[u8]) -> Result<(), ObjectStoreError> {
    let mut file = File::create_new(tmp).map_err(failed("create", tmp))?;
    file.write_all(&header(key)).map_err(failed("write", tmp))?;
    file.write_all(body).map_err(failed("write", tmp))?;
    file.sync_all().map_err(failed("sync", tmp))
}

/// Links `tmp` as `path`, which must not exist.
fn link(tmp: &Path, path: &Path, key: &ObjectKey) -> Result<(), ObjectStoreError> {
    match fs::hard_link(tmp, path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(ObjectStoreError::AlreadyExists(key.clone()))
        }
        linked => linked.map_err(failed("link", path)),
    }
}

/// The paths of the entries of `dir`.
fn entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect()
}

/// Syncs a directory, so the names created and removed in it are durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// A write refused for lack of free space.
#[derive(Debug, thiserror::Error)]
#[error(
    "{dir}: {free} bytes free, under the floor of {floor} bytes plus the {len} bytes of \
     this write (no eviction: the directory only grows)"
)]
struct Full {
    dir: PathBuf,
    free: u64,
    floor: u64,
    len: u64,
}

/// An I/O failure of a store call: the operation and the path. The cause is its
/// source, which [`ObjectStoreError::Transport`] prints after it.
#[derive(Debug, thiserror::Error)]
#[error("{op} {path}")]
struct Failed {
    op: &'static str,
    path: PathBuf,
    #[source]
    source: io::Error,
}

fn failed<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> ObjectStoreError + 'a {
    move |source| {
        ObjectStoreError::Transport(Arc::new(Failed {
            op,
            path: path.to_owned(),
            source,
        }))
    }
}

fn open_failed<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> FsStoreError + 'a {
    move |source| FsStoreError::Io {
        op,
        path: path.to_owned(),
        source,
    }
}

/// Runs `f` on tokio's blocking pool.
async fn blocking<T, F>(f: F) -> Result<T, ObjectStoreError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ObjectStoreError> + Send + 'static,
{
    // A panic in `f` is a bug; it is raised again here, as if `f` had run inline.
    tokio::task::spawn_blocking(f)
        .await
        .expect("a store task panicked")
}

impl ObjectStore for FsStore {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            conditional_put: true,
            object_lock: false,
        }
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        if retain_until.is_some() {
            return Err(ObjectStoreError::Unsupported("Object Lock retention"));
        }
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.put(&key, &body)).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.get(&key, range)).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.may_hold(&key)).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.delete(&key)).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let (inner, prefix) = (Arc::clone(&self.inner), prefix.clone());
        let after = after.map(|t| t.0.clone());
        let max = usize::from(max_keys.get());
        blocking(move || inner.list(&prefix, after.as_deref(), max)).await
    }
}
