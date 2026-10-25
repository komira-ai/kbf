//! A [`Backend`] over a directory of the local file system, for a tier that is a disk.

use std::io::{ErrorKind, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::{Buf, Bytes};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{Backend, ByteRange, Error, Listed, Meta, PartBody};

/// The length of the content tag that starts the file of an object: the SHA-256 of its body.
const TAG_LEN: usize = 32;

/// The longest file name, in bytes, that a key may be encoded to.
const NAME_LIMIT: usize = 255;

/// The size of the pieces in which the parts of an upload are joined.
const COPY_CHUNK: usize = 1 << 20;

/// The directory, inside the one of the backend, that holds the uploads in progress.
const UPLOADS: &str = ".uploads";

/// The file of an upload that holds the key it is for.
const KEY_FILE: &str = "key";

/// The number of the backends opened in this process, which tells their temporary files apart.
static OPENED: AtomicU64 = AtomicU64::new(0);

/// A bucket kept in a directory.
///
/// An object is one file of the directory. Its name is the key with `/` written as `~`; letters,
/// digits, `_`, `-` and a `.` (but not as the first byte) stay as they are, and every other
/// byte is written as `%` and two hex digits. So no key can name a file outside the directory
/// or a file the backend keeps for itself (their names start with `.`). A key whose name is
/// empty or longer than 255 bytes is refused with [`Error::Other`]. The file holds the SHA-256
/// of the body, which is the content tag, and then the body, so that a head reads 32 bytes and
/// not the object.
///
/// - A put writes a temporary file in the directory, syncs it, and hard-links it to the name,
///   which fails where the name is taken: the object appears whole or not at all, and a put of a
///   key that exists leaves it as it was. The directory needs a file system with hard links. A
///   crash leaves a `.tmp-` file behind, which is never listed and can be removed when the
///   process is stopped. The file is synced, the directory is not.
/// - A multipart upload is a directory `.uploads/<id>` that holds a file with the key and a file
///   for each part, written as the chunks of the part arrive, so that a part is never held in
///   memory; the completion joins the parts into the new object by streaming them, and removes
///   the upload. The uploads are files, so an upload outlives a restart of the process, and an
///   upload that is never completed or aborted stays until it is aborted.
///
/// The backend counts on being the only writer of its directory but for other backends over the
/// same directory (several proxies over one cold directory are fine: every change is atomic and
/// create-only, and the temporary files of two backends never have one name, since a name holds
/// the process id, the time the backend was opened and its number in the process);
/// [`Backend::put_if_match`] is atomic among the calls of one backend only, and so is the
/// replacement of a part by another of its number: the parts of one upload should be taken by one
/// backend.
pub struct DirBackend {
    root: PathBuf,
    /// Tells the temporary files of this backend from those of any other backend over the same
    /// directory, in this process or in another.
    nonce: String,
    /// Numbers the temporary files and the uploads.
    counter: AtomicU64,
    /// Held while an object is replaced, so that two replacements of one backend do not
    /// interleave their check and their write.
    replacing: Mutex<()>,
    /// Held while a part is moved to its name and the other files of its number are removed, so
    /// that two overlapping uploads of one part number do not remove each other's file.
    placing: Mutex<()>,
}

/// The file name of a key.
fn encode(key: &str) -> String {
    let mut name = String::with_capacity(key.len());
    for (at, byte) in key.bytes().enumerate() {
        match byte {
            b'/' => name.push('~'),
            b'.' if at > 0 => name.push('.'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => name.push(char::from(byte)),
            _ => name.push_str(&format!("%{byte:02X}")),
        }
    }
    name
}

/// The key a file name stands for, if the name is one [`encode`] makes.
fn decode(name: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(name.len());
    let mut rest = name.bytes();
    while let Some(byte) = rest.next() {
        match byte {
            b'~' => bytes.push(b'/'),
            b'%' => {
                let pair = [rest.next()?, rest.next()?];
                bytes.extend(hex::decode(pair).ok()?);
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).ok()
}

/// The error of a failed call on the file `path`.
fn failed(path: &Path, why: &std::io::Error) -> Error {
    let text = format!("{}: {why}", path.display());
    match why.kind() {
        ErrorKind::StorageFull => Error::Full(text),
        _ => Error::Other(text),
    }
}

/// Makes the error of a failed call on the file `path` from an I/O error.
fn io_error(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |why| failed(path, &why)
}

/// The error of a call on `path` that failed with `why`: `found` for the key if `why` is of the
/// kind `expected`, else as [`failed`].
fn classify(
    key: &str,
    path: &Path,
    why: &std::io::Error,
    expected: ErrorKind,
    found: fn(String) -> Error,
) -> Error {
    if why.kind() == expected {
        found(key.to_owned())
    } else {
        failed(path, why)
    }
}

/// The SHA-256 of `body`.
fn sha256(body: &[u8]) -> [u8; TAG_LEN] {
    finish(Sha256::new().chain_update(body))
}

/// The digest `hasher` has computed.
fn finish(hasher: Sha256) -> [u8; TAG_LEN] {
    let digest = hasher.finalize();
    let mut tag = [0_u8; TAG_LEN];
    tag.copy_from_slice(digest.as_ref());
    tag
}

/// When the file was last written, in milliseconds since the Unix epoch.
fn modified_of(meta: &std::fs::Metadata) -> u64 {
    let since = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    since.map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Whether the byte can be in the id of an upload, which names a directory.
fn is_id_byte(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'z' | b'-')
}

/// The name of the file of a part of an upload: its number and its content tag.
fn part_name(number: u32, tag: &str) -> String {
    format!("{number:05}-{tag}")
}

/// The number and the content tag a file name of a part has, if it is one.
fn parse_part(name: &str) -> Option<(u32, &str)> {
    let (number, tag) = name.split_once('-')?;
    Some((number.parse().ok()?, tag))
}

/// A temporary file that is removed when the guard is dropped: when the call that made it ends or
/// fails, and when the call is dropped half way, as the server drops the call of a client that
/// went away. The removal is not an error where the file was moved to its name.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Makes the new file at `path`, for writing. It is an error if the name is taken, so that two
/// writers never write to one file.
async fn create_new(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
}

/// Writes `data` to a new file at `path`, and syncs it to the disk.
async fn create_file(path: &Path, mut data: impl Buf) -> std::io::Result<()> {
    let mut file = create_new(path).await?;
    let written = file.write_all_buf(&mut data).await;
    let flushed = written.and(file.flush().await);
    flushed.and(file.sync_all().await)
}

/// Writes the chunks of `body` to a new file at `path`, and gives their SHA-256.
async fn write_stream(path: &Path, body: &mut PartBody<'_>) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut file = create_new(path).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(&fail)?;
    }
    let flushed = file.flush().await;
    flushed.and(file.sync_all().await).map_err(&fail)?;
    Ok(finish(hasher))
}

/// Writes the contents of `files`, one after the other, to a new file at `path` behind the
/// content tag of the whole, which is written last, and gives the tag.
async fn join_files(path: &Path, files: &[PathBuf]) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut out = create_new(path).await.map_err(&fail)?;
    out.write_all(&[0_u8; TAG_LEN]).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_CHUNK];
    for file in files {
        let mut input = File::open(file).await.map_err(io_error(file))?;
        loop {
            let count = input.read(&mut buffer).await.map_err(io_error(file))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            out.write_all(&buffer[..count]).await.map_err(&fail)?;
        }
    }
    let tag = finish(hasher);
    let sought = out.seek(SeekFrom::Start(0)).await.map(|_| ());
    let tagged = sought.and(out.write_all(&tag).await);
    let flushed = tagged.and(out.flush().await);
    flushed.and(out.sync_all().await).map_err(&fail)?;
    Ok(tag)
}

/// An object's file, opened, with its content tag read.
struct Stored {
    file: File,
    path: PathBuf,
    tag: [u8; TAG_LEN],
    size: u64,
    modified: u64,
}

/// Opens the file of an object and reads its content tag.
async fn open_stored(path: &Path) -> std::io::Result<Stored> {
    let mut file = File::open(path).await?;
    let meta = file.metadata().await?;
    let mut tag = [0_u8; TAG_LEN];
    file.read_exact(&mut tag).await?;
    Ok(Stored {
        file,
        path: path.to_owned(),
        tag,
        size: meta.len().saturating_sub(TAG_LEN as u64),
        modified: modified_of(&meta),
    })
}

/// A part that is stored in an upload.
struct StoredPart {
    number: u32,
    tag: String,
    name: String,
}

impl StoredPart {
    /// Whether this is the part `number` with the content tag `tag`.
    fn is(&self, number: u32, tag: &str) -> bool {
        (self.number, self.tag.as_str()) == (number, tag)
    }
}

/// The parts that are stored in the upload directory `dir`.
async fn stored_parts(dir: &Path) -> Result<Vec<StoredPart>, Error> {
    let fail = io_error(dir);
    let mut entries = fs::read_dir(dir).await.map_err(&fail)?;
    let mut parts = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(&fail)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((number, tag)) = parse_part(&name) {
            let tag = tag.to_owned();
            parts.push(StoredPart { number, tag, name });
        }
    }
    Ok(parts)
}

impl DirBackend {
    /// A bucket in the directory `root`, which is made if it does not exist.
    ///
    /// # Errors
    /// [`Error::Other`] if `root` is empty or the directory cannot be made.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            let text = "the directory of a tier has no name";
            return Err(Error::Other(text.to_owned()));
        }
        let uploads = root.join(UPLOADS);
        let made = std::fs::create_dir_all(&uploads);
        made.map_err(io_error(&uploads))?;
        let since = SystemTime::now().duration_since(UNIX_EPOCH);
        let nanos = since.map_or(0, |elapsed| elapsed.as_nanos());
        let opened = OPENED.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            root,
            nonce: format!("{}-{nanos:x}-{opened:x}", std::process::id()),
            counter: AtomicU64::new(0),
            replacing: Mutex::new(()),
            placing: Mutex::new(()),
        })
    }

    /// The path of the file of the object `key`.
    fn path(&self, key: &str) -> Result<PathBuf, Error> {
        let name = encode(key);
        if !(1..=NAME_LIMIT).contains(&name.len()) {
            let text = format!("key {key:?} is empty or too long to be a file name");
            return Err(Error::Other(text));
        }
        Ok(self.root.join(name))
    }

    /// A name for a temporary file in `dir`, which is not listed.
    fn temp(&self, dir: &Path) -> PathBuf {
        let number = self.counter.fetch_add(1, Ordering::SeqCst);
        dir.join(format!(".tmp-{}-{number}", self.nonce))
    }

    /// Opens the object `key`.
    async fn stored(&self, key: &str) -> Result<Stored, Error> {
        let path = self.path(key)?;
        let opened = open_stored(&path).await;
        let absent = ErrorKind::NotFound;
        opened.map_err(|why| classify(key, &path, &why, absent, Error::NotFound))
    }

    /// Writes the file of an object with this body to a new temporary file.
    async fn write_temp(&self, body: Bytes) -> Result<(PathBuf, [u8; TAG_LEN]), Error> {
        let tag = sha256(&body);
        let data = Bytes::copy_from_slice(&tag).chain(body);
        let temp = self.temp(&self.root);
        match create_file(&temp, data).await {
            Ok(()) => Ok((temp, tag)),
            Err(why) => {
                let _ = fs::remove_file(&temp).await;
                Err(failed(&temp, &why))
            }
        }
    }

    /// Moves the temporary file `temp` to `path` if nothing is there: a hard link fails where
    /// the name is taken. The temporary file is gone either way.
    async fn publish(&self, temp: &Path, path: &Path, key: &str) -> Result<(), Error> {
        let linked = fs::hard_link(temp, path).await;
        let _ = fs::remove_file(temp).await;
        let taken = ErrorKind::AlreadyExists;
        linked.map_err(|e| classify(key, path, &e, taken, Error::AlreadyExists))
    }

    /// The directory of the upload `id` of `key`.
    async fn upload_dir(&self, key: &str, id: &str) -> Result<PathBuf, Error> {
        let unknown = || Error::NotFound(id.to_owned());
        if !id.bytes().all(is_id_byte) {
            return Err(unknown());
        }
        let dir = self.root.join(UPLOADS).join(id);
        let named = fs::read_to_string(dir.join(KEY_FILE)).await;
        let named = named.map_err(|_| unknown())?;
        if named == key {
            Ok(dir)
        } else {
            Err(unknown())
        }
    }
}

#[async_trait]
impl Backend for DirBackend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let path = self.path(key)?;
        let (temp, _) = self.write_temp(body).await?;
        self.publish(&temp, &path, key).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let _replacing = self.replacing.lock().await;
        let current = self.stored(key).await?;
        if hex::encode(current.tag) != etag {
            return Err(Error::PreconditionFailed(key.to_owned()));
        }
        let (temp, tag) = self.write_temp(body).await?;
        let renamed = fs::rename(&temp, &current.path).await;
        let _ = fs::remove_file(&temp).await;
        renamed
            .map(|()| hex::encode(tag))
            .map_err(io_error(&current.path))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let mut stored = self.stored(key).await?;
        let size = stored.size;
        let (offset, len) = match range {
            None => (0, size),
            Some(range) if range.offset() > size => {
                let text = format!(
                    "range starting at {} is past the end of the {size}-byte object",
                    range.offset()
                );
                return Err(Error::Other(text));
            }
            Some(range) => (range.offset(), range.size().min(size - range.offset())),
        };
        let mut body = Vec::new();
        let start = SeekFrom::Start(TAG_LEN as u64 + offset);
        let sought = stored.file.seek(start).await;
        let read = (&mut stored.file).take(len).read_to_end(&mut body).await;
        sought.and(read).map_err(io_error(&stored.path))?;
        Ok(Bytes::from(body))
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let stored = self.stored(key).await?;
        Ok(Meta {
            size: stored.size,
            tag: hex::encode(stored.tag),
            modified: stored.modified,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        let path = self.path(key)?;
        match fs::remove_file(&path).await {
            Err(why) if why.kind() != ErrorKind::NotFound => Err(failed(&path, &why)),
            _ => Ok(()),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let fail = io_error(&self.root);
        let mut entries = fs::read_dir(&self.root).await.map_err(&fail)?;
        let mut found = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(&fail)? {
            let name = entry.file_name();
            let key = name
                .to_str()
                .filter(|name| !name.starts_with('.'))
                .and_then(decode)
                .filter(|key| key.starts_with(prefix));
            let Some(key) = key else {
                continue;
            };
            let meta = entry.metadata().await.ok();
            let Some(meta) = meta.filter(std::fs::Metadata::is_file) else {
                continue;
            };
            found.push(Listed {
                key,
                size: meta.len().saturating_sub(TAG_LEN as u64),
                modified: modified_of(&meta),
            });
        }
        found.sort();
        Ok(found)
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.path(key)?;
        let number = self.counter.fetch_add(1, Ordering::SeqCst);
        let since = SystemTime::now().duration_since(UNIX_EPOCH);
        let nanos = since.map_or(0, |elapsed| elapsed.as_nanos());
        let id = format!("u-{nanos:x}-{:x}-{number:x}", std::process::id());
        let dir = self.root.join(UPLOADS).join(&id);
        let made = fs::create_dir(&dir).await;
        let named = fs::write(dir.join(KEY_FILE), key).await;
        made.and(named).map_err(io_error(&dir))?;
        Ok(id)
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        mut body: PartBody<'_>,
    ) -> Result<String, Error> {
        let dir = self.upload_dir(key, id).await?;
        let temp = Scratch(self.temp(&dir));
        let tag = hex::encode(write_stream(&temp.0, &mut body).await?);
        let name = part_name(number, &tag);
        let _placing = self.placing.lock().await;
        let renamed = fs::rename(&temp.0, dir.join(&name)).await;
        renamed.map_err(io_error(&dir))?;
        let stored = stored_parts(&dir).await?;
        let others = stored.iter().filter(|part| part.number == number);
        for part in others.filter(|part| part.name != name) {
            let _ = fs::remove_file(dir.join(&part.name)).await;
        }
        Ok(tag)
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        let dir = self.upload_dir(key, id).await?;
        let path = self.path(key)?;
        let stored = stored_parts(&dir).await?;
        let mut files = Vec::new();
        for (number, tag) in parts {
            let found = stored.iter().find(|part| part.is(*number, tag));
            let Some(found) = found else {
                return Err(Error::InvalidPart(format!("part {number}")));
            };
            files.push(dir.join(&found.name));
        }
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Err(Error::AlreadyExists(key.to_owned()));
        }
        let temp = self.temp(&self.root);
        let tag = match join_files(&temp, &files).await {
            Ok(tag) => tag,
            Err(error) => {
                let _ = fs::remove_file(&temp).await;
                return Err(error);
            }
        };
        self.publish(&temp, &path, key).await?;
        let _ = fs::remove_dir_all(&dir).await;
        Ok(hex::encode(tag))
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        let dir = self.upload_dir(key, id).await?;
        fs::remove_dir_all(&dir).await.map_err(io_error(&dir))
    }
}

#[cfg(test)]
#[path = "dir_tests.rs"]
mod tests;
