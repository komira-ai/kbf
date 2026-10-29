//! Reading the action's inputs through the read grant: blobs by digest, checked, and the input
//! tree laid out under a directory.
//!
//! A [`BlobSource`] returns the bytes of one blob. Nothing a source returns is trusted:
//! [`BlobSource::verified`] checks the length and the SHA-256 of every blob against its digest, and
//! every read in this module goes through it. [`HttpBlobSource`] is the source over the read grant
//! (a base URL and a bearer token); tests use an in-memory one.
//!
//! [`materialize`] writes the tree under an input root digest into a directory. Directory messages
//! come from clients, so every name is checked before it touches the filesystem: a name with a
//! slash, `.`, `..` or a NUL, empty, or used twice in one directory refuses the tree. Files are
//! created with `O_EXCL` in directories this module made, so no write follows a symlink the tree
//! contains. There is no cache and no hard link: each file is its own copy.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use prost::Message;
use sha2::{Digest as _, Sha256};

/// Why an input could not be read or laid out.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// A digest is not a lowercase SHA-256 hex string with a non-negative size.
    #[error("invalid digest {0:?}")]
    BadDigest(String),
    /// The source could not give the blob: unreachable, refused, or absent.
    #[error("blob {digest}: {why}")]
    Source {
        /// The digest asked for, as `<hex>/<size>`.
        digest: String,
        /// What went wrong.
        why: String,
    },
    /// The source gave bytes that are not the blob its digest names.
    #[error("blob {digest} does not match its digest: {why}")]
    Mismatch {
        /// The digest asked for, as `<hex>/<size>`.
        digest: String,
        /// How the bytes differ.
        why: String,
    },
    /// The tree breaks a rule: the client's error.
    #[error("{0}")]
    Invalid(String),
    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path that was being made or written.
        path: PathBuf,
        /// The operating system's error.
        #[source]
        source: std::io::Error,
    },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> FetchError + '_ {
    move |source| FetchError::Io {
        path: path.to_owned(),
        source,
    }
}

/// `<hex>/<size>`: how a digest is written in URLs, errors and the manifest.
#[must_use]
pub fn label(digest: &Digest) -> String {
    format!("{}/{}", digest.hash, digest.size_bytes)
}

/// Refuses a digest that is not 64 lowercase hex digits with a size of at least 0.
fn check_digest(digest: &Digest) -> Result<(), FetchError> {
    let hex = digest.hash.len() == 64
        && digest
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex && digest.size_bytes >= 0 {
        Ok(())
    } else {
        Err(FetchError::BadDigest(label(digest)))
    }
}

/// Where blobs come from. Implement [`BlobSource::get`]; read through [`BlobSource::verified`].
pub trait BlobSource {
    /// The bytes the source holds for `digest`, unchecked.
    ///
    /// # Errors
    /// The blob cannot be had: [`FetchError::Source`].
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError>;

    /// The blob `digest` names: [`BlobSource::get`] with the digest itself checked first, and the
    /// bytes checked against it after (length, then SHA-256).
    ///
    /// # Errors
    /// The digest is malformed, the source fails, or the bytes are not the blob.
    fn verified(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        check_digest(digest)?;
        let bytes = self.get(digest)?;
        let mismatch = |why: String| FetchError::Mismatch {
            digest: label(digest),
            why,
        };
        if i64::try_from(bytes.len()).ok() != Some(digest.size_bytes) {
            return Err(mismatch(format!("it has {} bytes", bytes.len())));
        }
        let hash = hex::encode(Sha256::digest(&bytes));
        if hash != digest.hash {
            return Err(mismatch(format!("its SHA-256 is {hash}")));
        }
        Ok(bytes)
    }
}

/// Fetches a blob and decodes it as `M`.
///
/// # Errors
/// The blob cannot be read ([`BlobSource::verified`]) or does not decode as `M`
/// ([`FetchError::Invalid`]).
pub fn fetch_message<M: Message + Default>(
    source: &impl BlobSource,
    digest: &Digest,
) -> Result<M, FetchError> {
    let bytes = source.verified(digest)?;
    M::decode(bytes.as_slice())
        .map_err(|e| FetchError::Invalid(format!("blob {} does not decode: {e}", label(digest))))
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// The blobs of a read grant: `GET <base>/<hex>/<size>` with `Authorization: Bearer <token>`,
/// over HTTPS with rustls on the ring provider, trusting only the roots it is given. No redirect
/// is followed and no proxy is used.
pub struct HttpBlobSource {
    agent: ureq::Agent,
    base: String,
    authorization: String,
}

impl std::fmt::Debug for HttpBlobSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpBlobSource")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// The host's trusted root certificates, DER encoded: what to pass to [`HttpBlobSource::new`]
/// when the grant's server uses a public CA.
#[must_use]
pub fn native_roots() -> Vec<Vec<u8>> {
    rustls_native_certs::load_native_certs()
        .certs
        .into_iter()
        .map(|c| c.as_ref().to_vec())
        .collect()
}

impl HttpBlobSource {
    /// A source for the grant at `base` (an `https://` URL; a trailing `/` is dropped), with
    /// `token` as the bearer and `roots` (DER certificates) as the only trusted roots.
    ///
    /// # Errors
    /// `roots` is empty: every connection would fail with an unhelpful message.
    pub fn new(base: &str, token: &str, roots: &[Vec<u8>]) -> Result<Self, FetchError> {
        if roots.is_empty() {
            return Err(FetchError::Invalid(
                "no trusted root certificates for the read grant".to_owned(),
            ));
        }
        Ok(Self::build(base, token, roots, true))
    }

    fn build(base: &str, token: &str, roots: &[Vec<u8>], https_only: bool) -> Self {
        let roots: Vec<ureq::tls::Certificate<'static>> = roots
            .iter()
            .map(|der| ureq::tls::Certificate::from_der(der).to_owned())
            .collect();
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .https_only(https_only)
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .tls_config(tls)
            .build()
            .into();
        Self {
            agent,
            base: base.trim_end_matches('/').to_owned(),
            authorization: format!("Bearer {token}"),
        }
    }
}

impl BlobSource for HttpBlobSource {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        check_digest(digest)?;
        let source = |why: String| FetchError::Source {
            digest: label(digest),
            why,
        };
        let url = format!("{}/{}", self.base, label(digest));
        let mut response = self
            .agent
            .get(&url)
            .header("Authorization", &self.authorization)
            .call()
            .map_err(|e| source(e.to_string()))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(source(format!("the grant answered {status}")));
        }
        // One byte past the size is enough for `verified` to see a blob that is too long.
        let limit = digest.size_bytes.unsigned_abs() + 1;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(|e| source(e.to_string()))?;
        Ok(bytes)
    }
}

/// Checks one name of a Directory entry.
fn check_name(name: &str) -> Result<(), FetchError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(FetchError::Invalid(format!(
            "input tree entry name {name:?} is not a single path component"
        )));
    }
    Ok(())
}

/// Checks a path the Command names (working directory, output path): relative, made of
/// non-empty components other than `.` and `..`, joined by single slashes. The empty path is the
/// directory itself.
///
/// # Errors
/// [`FetchError::Invalid`], naming `what`.
pub fn check_relative(what: &str, path: &str) -> Result<(), FetchError> {
    let plain = path.is_empty()
        || path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'));
    if !plain {
        return Err(FetchError::Invalid(format!(
            "{what} {path:?} must be relative and must not contain . or .."
        )));
    }
    Ok(())
}

/// Refuses a Directory whose entry names are not single path components, or that names one entry
/// twice.
fn check_names(directory: &kbf_proto::reapi::Directory) -> Result<(), FetchError> {
    let mut seen = BTreeSet::new();
    let names = directory
        .files
        .iter()
        .map(|f| &f.name)
        .chain(directory.directories.iter().map(|d| &d.name))
        .chain(directory.symlinks.iter().map(|s| &s.name));
    for name in names {
        check_name(name)?;
        if !seen.insert(name) {
            return Err(FetchError::Invalid(format!(
                "input tree names {name:?} twice in one directory"
            )));
        }
    }
    Ok(())
}

/// Where one file blob goes: each path it is written to, and whether that copy is executable.
struct Copies {
    digest: Digest,
    copies: Vec<(PathBuf, bool)>,
}

/// Writes the tree under `root` into `dir` (created if absent, and empty).
///
/// The tree is walked first: every Directory message is read once however many places name it,
/// every name is checked, every directory and symlink made. Then each distinct file blob is read
/// once and written, with its executable bit, to every path that names it. A blob that is
/// missing or does not match its digest fails the call; files already written stay in `dir`, for
/// the caller to remove.
///
/// # Errors
/// A blob is missing or corrupt, the tree breaks a naming rule, or a write fails.
pub fn materialize(source: &impl BlobSource, root: &Digest, dir: &Path) -> Result<(), FetchError> {
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let files = make_directories(source, root, dir)?;
    write_files(source, files)
}

/// Walks the tree under `root`, checking every Directory's names and making its directories and
/// symlinks under `dir`; returns the file blobs to write, each distinct digest once.
fn make_directories(
    source: &impl BlobSource,
    root: &Digest,
    dir: &Path,
) -> Result<Vec<Copies>, FetchError> {
    let mut files: Vec<Copies> = Vec::new();
    let mut blob_at: HashMap<String, usize> = HashMap::new();
    let mut loaded: HashMap<String, kbf_proto::reapi::Directory> = HashMap::new();
    let mut pending = vec![(root.clone(), dir.to_owned())];
    while let Some((digest, here)) = pending.pop() {
        let key = label(&digest);
        if !loaded.contains_key(&key) {
            let directory = fetch_message(source, &digest)?;
            check_names(&directory)?;
            loaded.insert(key.clone(), directory);
        }
        let directory = &loaded[&key];
        for file in &directory.files {
            let digest = file.digest.clone().ok_or_else(|| {
                FetchError::Invalid(format!("input file {:?} has no digest", file.name))
            })?;
            let copy = (here.join(&file.name), file.is_executable);
            let at = *blob_at.entry(label(&digest)).or_insert_with(|| {
                files.push(Copies {
                    digest,
                    copies: Vec::new(),
                });
                files.len() - 1
            });
            files[at].copies.push(copy);
        }
        for link in &directory.symlinks {
            let path = here.join(&link.name);
            std::os::unix::fs::symlink(&link.target, &path).map_err(io(&path))?;
        }
        for sub in &directory.directories {
            let path = here.join(&sub.name);
            let digest = sub.digest.clone().ok_or_else(|| {
                FetchError::Invalid(format!("input directory {:?} has no digest", sub.name))
            })?;
            std::fs::create_dir(&path).map_err(io(&path))?;
            pending.push((digest, path));
        }
    }
    Ok(files)
}

/// Reads each blob in `files` once and writes it to every path that names it.
fn write_files(source: &impl BlobSource, files: Vec<Copies>) -> Result<(), FetchError> {
    for blob in files {
        let bytes = source.verified(&blob.digest)?;
        for (path, is_executable) in &blob.copies {
            create_file(path, *is_executable, &bytes)?;
        }
    }
    Ok(())
}

/// Creates `path` with `O_EXCL`, writes `bytes`, and sets the mode exactly (the process umask
/// does not apply).
fn create_file(path: &Path, is_executable: bool, bytes: &[u8]) -> Result<(), FetchError> {
    let mode = if is_executable { 0o755 } else { 0o644 };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(io(path))?;
    file.write_all(bytes).map_err(io(path))?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io(path))
}

#[cfg(test)]
mod tests {
    use kbf_testkit::dir::TestDir;
    use std::collections::HashMap;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use kbf_proto::reapi::{Directory, DirectoryNode, FileNode, SymlinkNode};

    use super::*;

    fn digest_of(bytes: &[u8]) -> Digest {
        Digest {
            hash: hex::encode(Sha256::digest(bytes)),
            size_bytes: bytes.len() as i64,
        }
    }

    /// Blobs by hash; `reads` counts the `get`s per hash.
    #[derive(Default)]
    struct Memory {
        blobs: HashMap<String, Vec<u8>>,
        reads: Mutex<HashMap<String, usize>>,
    }

    impl Memory {
        fn put(&mut self, bytes: Vec<u8>) -> Digest {
            let digest = digest_of(&bytes);
            self.blobs.insert(digest.hash.clone(), bytes);
            digest
        }

        fn put_directory(&mut self, directory: &Directory) -> Digest {
            self.put(directory.encode_to_vec())
        }

        fn reads(&self, digest: &Digest) -> usize {
            self.reads
                .lock()
                .expect("reads")
                .get(&digest.hash)
                .copied()
                .unwrap_or(0)
        }
    }

    impl BlobSource for Memory {
        fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
            *self
                .reads
                .lock()
                .expect("reads")
                .entry(digest.hash.clone())
                .or_default() += 1;
            self.blobs
                .get(&digest.hash)
                .cloned()
                .ok_or_else(|| FetchError::Source {
                    digest: label(digest),
                    why: "absent".to_owned(),
                })
        }
    }

    fn scratch(name: &str) -> TestDir {
        TestDir::under(std::env::temp_dir(), &format!("kbf-task-fetch-{name}"))
    }

    fn file(name: &str, digest: &Digest, is_executable: bool) -> FileNode {
        FileNode {
            name: name.to_owned(),
            digest: Some(digest.clone()),
            is_executable,
            ..FileNode::default()
        }
    }

    fn sub(name: &str, digest: &Digest) -> DirectoryNode {
        DirectoryNode {
            name: name.to_owned(),
            digest: Some(digest.clone()),
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    /// root: bin/run (executable), a.txt, copy.txt (same blob as a.txt), link -> a.txt,
    /// twice/one and twice/two (the same Directory), deep/x/y/z.txt.
    fn tree() -> (Memory, Digest) {
        let mut mem = Memory::default();
        let text = mem.put(b"hello".to_vec());
        let script = mem.put(b"tool bytes\n".to_vec());
        let empty = mem.put(Vec::new());
        let bin = mem.put_directory(&Directory {
            files: vec![file("run", &script, true)],
            ..Directory::default()
        });
        let shared = mem.put_directory(&Directory {
            files: vec![file("e", &empty, false)],
            ..Directory::default()
        });
        let z = mem.put_directory(&Directory {
            files: vec![file("z.txt", &text, false)],
            ..Directory::default()
        });
        let y = mem.put_directory(&Directory {
            directories: vec![sub("y", &z)],
            ..Directory::default()
        });
        let x = mem.put_directory(&Directory {
            directories: vec![sub("x", &y)],
            ..Directory::default()
        });
        let root = mem.put_directory(&Directory {
            files: vec![file("a.txt", &text, false), file("copy.txt", &text, false)],
            directories: vec![
                sub("bin", &bin),
                sub("one", &shared),
                sub("two", &shared),
                sub("deep", &x),
            ],
            symlinks: vec![SymlinkNode {
                name: "link".to_owned(),
                target: "a.txt".to_owned(),
                ..SymlinkNode::default()
            }],
            ..Directory::default()
        });
        (mem, root)
    }

    /// Catches a tree laid out wrongly: a nested directory, the executable bit and the plain
    /// mode, a symlink kept as one, two files of one blob each filled, and a blob or Directory
    /// read more than once.
    #[test]
    fn a_nested_tree_is_laid_out_with_modes_and_symlinks() {
        let (mem, root) = tree();
        let scratch_dir = scratch("layout");
        let dir = scratch_dir.join("in");
        materialize(&mem, &root, &dir).expect("materialize");
        assert_eq!(std::fs::read(dir.join("a.txt")).expect("a"), b"hello");
        assert_eq!(std::fs::read(dir.join("copy.txt")).expect("copy"), b"hello");
        assert_eq!(
            std::fs::read(dir.join("deep/x/y/z.txt")).expect("z"),
            b"hello"
        );
        assert_eq!(
            std::fs::read(dir.join("bin/run")).expect("run"),
            b"tool bytes\n"
        );
        assert_eq!(mode(&dir.join("bin/run")), 0o755);
        assert_eq!(mode(&dir.join("a.txt")), 0o644);
        assert_eq!(
            std::fs::read_link(dir.join("link")).expect("link"),
            Path::new("a.txt")
        );
        assert!(dir.join("one/e").is_file() && dir.join("two/e").is_file());
        assert_eq!(mem.reads(&digest_of(b"hello")), 1);
        let shared = digest_of(
            &Directory {
                files: vec![file("e", &digest_of(b""), false)],
                ..Directory::default()
            }
            .encode_to_vec(),
        );
        assert_eq!(mem.reads(&shared), 1);
    }

    /// Catches a blob accepted though its bytes are not the ones its digest names: wrong
    /// content of the right length, wrong length, and a missing blob.
    #[test]
    fn a_blob_that_does_not_match_its_digest_is_refused() {
        let mut mem = Memory::default();
        let good = digest_of(b"abc");
        mem.blobs.insert(good.hash.clone(), b"abd".to_vec());
        let error = mem.verified(&good).expect_err("same length, other bytes");
        assert!(matches!(&error, FetchError::Mismatch { .. }), "{error:?}");
        assert!(error.to_string().contains(&label(&good)), "{error}");
        assert!(error.to_string().contains("SHA-256"), "{error}");

        mem.blobs.insert(good.hash.clone(), b"abcd".to_vec());
        let error = mem.verified(&good).expect_err("longer");
        assert!(error.to_string().contains("4 bytes"), "{error}");

        let absent = digest_of(b"nothing");
        let error = mem.verified(&absent).expect_err("absent");
        assert!(matches!(error, FetchError::Source { .. }), "{error:?}");

        mem.blobs.insert(good.hash.clone(), b"abc".to_vec());
        assert_eq!(mem.verified(&good).expect("good"), b"abc");
    }

    /// Catches a corrupt blob inside a tree being written: the file blob is checked, and so is a
    /// Directory message.
    #[test]
    fn a_corrupt_blob_in_a_tree_fails_materialize() {
        let (mut mem, root) = tree();
        let text = digest_of(b"hello");
        mem.blobs.insert(text.hash.clone(), b"HELLO".to_vec());
        let error = materialize(&mem, &root, &scratch("corrupt-file")).expect_err("file");
        assert!(matches!(error, FetchError::Mismatch { .. }), "{error:?}");

        let (mut mem, root) = tree();
        mem.blobs.insert(root.hash.clone(), b"junk".to_vec());
        let error = materialize(&mem, &root, &scratch("corrupt-dir")).expect_err("directory");
        assert!(matches!(error, FetchError::Mismatch { .. }), "{error:?}");
    }

    /// Catches a Directory entry name that could write outside the directory or collide:
    /// slash, `..`, `.`, empty, NUL, a name twice (also across files and directories).
    #[test]
    fn a_bad_name_is_refused() {
        let names = ["a/b", "..", ".", "", "a\0b", "dup"];
        for (i, name) in names.iter().enumerate() {
            let mut mem = Memory::default();
            let blob = mem.put(b"x".to_vec());
            let empty = mem.put_directory(&Directory::default());
            let mut directory = Directory {
                files: vec![file(name, &blob, false)],
                ..Directory::default()
            };
            if *name == "dup" {
                directory.directories.push(sub("dup", &empty));
            }
            let root = mem.put_directory(&directory);
            let error = materialize(&mem, &root, &scratch(&format!("name{i}"))).expect_err(name);
            assert!(
                matches!(error, FetchError::Invalid(_)),
                "{name:?}: {error:?}"
            );
        }
    }

    /// Catches an entry without a digest, and a blob that is not a Directory where one is wanted.
    #[test]
    fn a_malformed_tree_is_refused() {
        let mut mem = Memory::default();
        let no_file_digest = mem.put_directory(&Directory {
            files: vec![FileNode {
                name: "f".to_owned(),
                ..FileNode::default()
            }],
            ..Directory::default()
        });
        let no_dir_digest = mem.put_directory(&Directory {
            directories: vec![DirectoryNode {
                name: "d".to_owned(),
                digest: None,
            }],
            ..Directory::default()
        });
        let not_a_directory = mem.put(vec![0xff, 0xff, 0xff]);
        for (i, root) in [no_file_digest, no_dir_digest, not_a_directory]
            .iter()
            .enumerate()
        {
            let error =
                materialize(&mem, root, &scratch(&format!("malformed{i}"))).expect_err("bad");
            assert!(matches!(error, FetchError::Invalid(_)), "{i}: {error:?}");
        }
    }

    /// Catches a tree written over something already there (`O_EXCL`), and an unwritable target.
    #[test]
    fn writing_does_not_overwrite_or_follow() {
        let (mem, root) = tree();
        let dir = scratch("exists");
        std::fs::write(dir.join("a.txt"), b"mine").expect("pre-existing");
        let error = materialize(&mem, &root, &dir).expect_err("exists");
        assert!(matches!(error, FetchError::Io { .. }), "{error:?}");
        assert_eq!(std::fs::read(dir.join("a.txt")).expect("kept"), b"mine");

        // The directory itself cannot be made under a file.
        let under_file = dir.join("a.txt").join("in");
        let error = materialize(&mem, &root, &under_file).expect_err("under a file");
        assert!(matches!(error, FetchError::Io { .. }), "{error:?}");

        // A symlink in the tree cannot be made where a directory entry already is.
        let dir = scratch("link-exists");
        std::fs::write(dir.join("link"), b"x").expect("pre-existing");
        let error = materialize(&mem, &root, &dir).expect_err("link exists");
        assert!(matches!(error, FetchError::Io { .. }), "{error:?}");
    }

    /// Catches a digest that could reach a URL or path unchecked.
    #[test]
    fn a_malformed_digest_is_refused() {
        let mem = Memory::default();
        let good = digest_of(b"x");
        let cases = [
            Digest {
                hash: "../etc".to_owned(),
                size_bytes: 1,
            },
            Digest {
                hash: good.hash.to_uppercase(),
                size_bytes: 1,
            },
            Digest {
                hash: good.hash.clone(),
                size_bytes: -1,
            },
        ];
        for digest in cases {
            let error = mem.verified(&digest).expect_err("bad digest");
            assert!(matches!(error, FetchError::BadDigest(_)), "{error:?}");
        }
    }

    /// Catches working-directory and output paths that leave the tree.
    #[test]
    fn relative_paths_are_checked() {
        for ok in ["", "a", "a/b.c", "a/.hidden"] {
            check_relative("path", ok).expect(ok);
        }
        for bad in ["/a", "a//b", "a/", ".", "..", "a/../b", "a/./b", "a\0b"] {
            let error = check_relative("output path", bad).expect_err(bad);
            assert!(error.to_string().contains("output path"), "{error}");
        }
    }

    /// One request the test server saw: its request line and its Authorization header.
    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    /// Serves `answers` (status line and body), one connection each, in order, over plain HTTP;
    /// returns the base URL and what it saw.
    fn serve(answers: Vec<(&'static str, Vec<u8>)>) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let seen: Seen = Arc::default();
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for (status, body) in answers {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let (mut line, mut auth) = (String::new(), String::new());
                reader.read_line(&mut line).expect("request line");
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).expect("header");
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = header.strip_prefix("authorization: ") {
                        value.trim().clone_into(&mut auth);
                    }
                }
                log.lock()
                    .expect("log")
                    .push((line.trim().to_owned(), auth));
                let mut stream = stream;
                // The client may hang up on a body it has read enough of.
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (base, seen)
    }

    fn plain(base: &str) -> HttpBlobSource {
        HttpBlobSource::build(&format!("{base}/"), "tok", &[], false)
    }

    /// Catches a wrong URL shape or a missing bearer: the grant sees `GET /<hex>/<size>` and the
    /// token, the trailing slash of the base does not double, and the body comes back whole.
    #[test]
    fn the_http_source_asks_the_grant_with_the_token() {
        let blob = b"payload".to_vec();
        let digest = digest_of(&blob);
        let (base, seen) = serve(vec![("200 OK", blob.clone())]);
        let got = plain(&base).verified(&digest).expect("fetch");
        assert_eq!(got, blob);
        let seen = seen.lock().expect("seen");
        assert_eq!(
            seen[0],
            (
                format!("GET /{}/{} HTTP/1.1", digest.hash, digest.size_bytes),
                "Bearer tok".to_owned()
            )
        );
    }

    /// Catches a non-200 taken for a blob, a body longer than its digest taken whole, and an
    /// unreachable grant reported as anything but a source error naming the digest.
    #[test]
    fn the_http_source_refuses_bad_answers() {
        let digest = digest_of(b"abc");
        let (base, _) = serve(vec![
            ("403 Forbidden", b"no".to_vec()),
            ("200 OK", vec![b'a'; 100_000]),
        ]);
        let source = plain(&base);
        let error = source.verified(&digest).expect_err("403");
        assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
        assert!(error.to_string().contains("403"), "{error}");
        assert!(error.to_string().contains(&label(&digest)), "{error}");
        let error = source.verified(&digest).expect_err("too long");
        assert!(error.to_string().contains("4 bytes"), "{error}");

        let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", closed.local_addr().expect("addr"));
        drop(closed);
        let error = plain(&base).verified(&digest).expect_err("unreachable");
        assert!(matches!(error, FetchError::Source { .. }), "{error:?}");
    }

    /// Catches a digest reaching the URL unchecked when `get` is called directly.
    #[test]
    fn the_http_source_checks_the_digest_it_is_asked_for() {
        let bad = Digest {
            hash: "x/../y".to_owned(),
            size_bytes: 1,
        };
        let error = plain("http://127.0.0.1:1").get(&bad).expect_err("bad");
        assert!(matches!(error, FetchError::BadDigest(_)), "{error:?}");
    }

    /// Catches a source built with no roots to trust, and the redacted Debug.
    #[test]
    fn the_https_source_needs_roots_and_hides_its_token() {
        let error = HttpBlobSource::new("https://grant.example", "secret", &[]).expect_err("none");
        assert!(matches!(error, FetchError::Invalid(_)), "{error:?}");
        let source = HttpBlobSource::new("https://grant.example/", "secret", &[vec![1, 2, 3]])
            .expect("one root");
        let shown = format!("{source:?}");
        assert!(shown.contains("https://grant.example"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
        // Plain HTTP is refused by a source built with `new`.
        let digest = digest_of(b"x");
        let error = HttpBlobSource::new("http://127.0.0.1:1", "t", &[vec![1]])
            .expect("source")
            .get(&digest)
            .expect_err("http");
        assert!(matches!(error, FetchError::Source { .. }), "{error:?}");
        let _ = native_roots();
    }
}
