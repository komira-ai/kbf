//! Collecting an action's declared outputs, recording each blob instead of uploading it.
//!
//! [`collect_outputs`] reads the outputs the way the container driver does
//! ([`kbf_outputs::collect`]: files, directories as Trees, symlinks, the executable bit, nothing
//! followed through a symlink, within [`OutputLimits`]) and hashes the stdout and stderr files. The
//! [`Store`] it hands `kbf-outputs` is a [`Recorder`]: where the driver's store uploads, this one
//! keeps each distinct blob, by digest, for [`crate::pack`] to read back. A Directory or Tree blob,
//! and a file of at most one chunk, is kept in memory; a larger file is kept as the open descriptor
//! `kbf-outputs` hands over, so it is read from the file it hashed and not from a path that could
//! since have changed.
//!
//! The blobs are bounded by a byte cap (the pack's): the recorder refuses a blob that would take the
//! distinct blobs past it, so a tree of outputs too big for one pack fails here, with the cap named,
//! and does not fill the job's memory first.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

use kbf_outputs::{OutputsError, Store, StoreError, digest_of, store_file};
use kbf_proto::reapi::{ActionResult, Digest};
use sha2::{Digest as _, Sha256};

pub use kbf_outputs::OutputLimits;

/// How many blobs the recorder accepts at once; it does no I/O, so any small number serves.
const IN_FLIGHT: usize = 4;

/// Where one recorded blob's bytes are.
#[derive(Debug)]
enum Blob {
    Bytes(Vec<u8>),
    /// A file held open, its bytes starting at `start`.
    File {
        file: File,
        start: u64,
    },
}

/// The blobs of one action's outputs, stdout and stderr, each distinct digest once.
#[derive(Debug, Default)]
pub struct Blobs {
    blobs: BTreeMap<(String, i64), Blob>,
    bytes: u64,
}

impl Blobs {
    /// How many distinct blobs are recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    /// Whether no blob is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }

    /// Blobs of the given bytes held in memory, for tests.
    #[cfg(test)]
    pub(crate) fn from_bytes(all: &[&[u8]]) -> Self {
        let mut blobs = Self::default();
        for bytes in all {
            let digest = digest_of(bytes);
            blobs.bytes += bytes.len() as u64;
            blobs.blobs.insert(
                (digest.hash, digest.size_bytes),
                Blob::Bytes(bytes.to_vec()),
            );
        }
        blobs
    }

    /// The sum of the sizes of the distinct blobs.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.bytes
    }

    /// Every recorded digest, in order of (hash, size).
    pub fn digests(&self) -> impl Iterator<Item = Digest> + '_ {
        self.blobs.keys().map(|(hash, size)| Digest {
            hash: hash.clone(),
            size_bytes: *size,
        })
    }

    /// Whether `digest` is recorded.
    #[must_use]
    pub fn contains(&self, digest: &Digest) -> bool {
        self.blobs
            .contains_key(&(digest.hash.clone(), digest.size_bytes))
    }

    /// The bytes recorded under `digest`, or `None` if it is not recorded. A file is read in full
    /// from where it was recorded.
    ///
    /// # Errors
    /// The held file cannot be read to its recorded size.
    pub fn read(&self, digest: &Digest) -> std::io::Result<Option<Cow<'_, [u8]>>> {
        let Some(blob) = self.blobs.get(&(digest.hash.clone(), digest.size_bytes)) else {
            return Ok(None);
        };
        Ok(Some(match blob {
            Blob::Bytes(bytes) => Cow::Borrowed(bytes.as_slice()),
            Blob::File { file, start } => {
                // Recorded sizes are non-negative: they come from a hashed length.
                let mut bytes = vec![0; usize::try_from(digest.size_bytes).unwrap_or(0)];
                file.read_exact_at(&mut bytes, *start)?;
                Cow::Owned(bytes)
            }
        }))
    }

    /// A reader over the bytes recorded under `digest`, from the first byte; the blob is not
    /// loaded.
    ///
    /// # Errors
    /// `NotFound` when `digest` is not recorded.
    pub fn reader(&self, digest: &Digest) -> std::io::Result<BlobReader<'_>> {
        let blob = self
            .blobs
            .get(&(digest.hash.clone(), digest.size_bytes))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        Ok(BlobReader {
            source: match blob {
                Blob::Bytes(bytes) => Source::Bytes(bytes),
                Blob::File { file, start } => Source::File {
                    file,
                    next: *start,
                    end: *start + digest.size_bytes.unsigned_abs(),
                },
            },
            hasher: Sha256::new(),
        })
    }
}

/// A reader over one recorded blob's bytes from the start. A held file is read where it was
/// recorded with `pread`, never loaded whole, and every [`Blobs::reader`] call starts again from
/// the first byte, so a retried PUT re-sends the whole blob. The bytes read are hashed as they
/// pass, so [`BlobReader::matches`] says whether what was streamed is the blob that was recorded.
#[derive(Debug)]
pub struct BlobReader<'a> {
    source: Source<'a>,
    hasher: Sha256,
}

#[derive(Debug)]
enum Source<'a> {
    /// Bytes held in memory.
    Bytes(&'a [u8]),
    /// A held file: the next offset to read and where the blob ends.
    File { file: &'a File, next: u64, end: u64 },
}

impl BlobReader<'_> {
    /// Whether the bytes read so far hash to `digest`. Call it after reading to the end.
    #[must_use]
    pub fn matches(self, digest: &Digest) -> bool {
        hex::encode(self.hasher.finalize()) == digest.hash
    }
}

impl Read for BlobReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = match &mut self.source {
            Source::Bytes(bytes) => bytes.read(buf)?,
            Source::File { file, next, end } => {
                let want = buf
                    .len()
                    .min(usize::try_from(*end - *next).unwrap_or(usize::MAX));
                let n = file.read_at(&mut buf[..want], *next)?;
                *next += n as u64;
                n
            }
        };
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// A [`Store`] that keeps what it is given, up to a byte cap, and uploads nothing.
struct Recorder {
    cap: u64,
    blobs: Mutex<Blobs>,
}

impl Recorder {
    fn new(cap: u64) -> Self {
        Self {
            cap,
            blobs: Mutex::new(Blobs::default()),
        }
    }

    fn record(&self, digest: &Digest, blob: impl FnOnce() -> Blob) -> Result<(), StoreError> {
        let mut blobs = self
            .blobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (digest.hash.clone(), digest.size_bytes);
        if blobs.blobs.contains_key(&key) {
            return Ok(());
        }
        let size = u64::try_from(digest.size_bytes).unwrap_or(0);
        if blobs.bytes.saturating_add(size) > self.cap {
            return Err(format!(
                "output-too-large: the outputs pass the {} bytes the run's packs and large objects hold",
                self.cap
            )
            .into());
        }
        blobs.bytes += size;
        blobs.blobs.insert(key, blob());
        Ok(())
    }
}

impl Store for Recorder {
    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        let digest = digest_of(&bytes);
        self.record(&digest, || Blob::Bytes(bytes))?;
        Ok(digest)
    }

    async fn put_file(&self, mut file: File, digest: Digest) -> Result<Digest, StoreError> {
        let start = file.stream_position()?;
        self.record(&digest, || Blob::File { file, start })?;
        Ok(digest)
    }

    fn in_flight(&self) -> usize {
        IN_FLIGHT
    }
}

/// What an action left: its result as `kbf-outputs` fills it, and the blobs it names.
#[derive(Debug)]
pub struct Outputs {
    /// The output files, directories and symlinks, and the stdout and stderr digests. The exit code
    /// and the timings are the caller's to set.
    pub result: ActionResult,
    /// Every blob the result names, and each Directory of a Tree's, stored once.
    pub blobs: Blobs,
}

/// Collects the outputs `paths` (relative to `working_directory`) under `root`, and the files
/// `stdout` and `stderr`, within `limits`, recording at most `cap` bytes of distinct blobs.
///
/// # Errors
/// An [`OutputsError`]: a bad output path, a limit passed, an unreadable output or log, or blobs
/// that pass `cap`.
pub async fn collect_outputs(
    root: &Path,
    working_directory: &str,
    paths: &[String],
    stdout: &Path,
    stderr: &Path,
    limits: OutputLimits,
    cap: u64,
) -> Result<Outputs, OutputsError> {
    let recorder = Recorder::new(cap);
    let mut result = ActionResult::default();
    kbf_outputs::collect(
        &recorder,
        root,
        working_directory,
        paths,
        limits,
        &mut result,
    )
    .await?;
    result.stdout_digest = Some(store_file(&recorder, stdout, limits.max_stdio_bytes).await?);
    result.stderr_digest = Some(store_file(&recorder, stderr, limits.max_stdio_bytes).await?);
    let blobs = recorder
        .blobs
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(Outputs { result, blobs })
}

#[cfg(test)]
mod tests {
    use kbf_testkit::dir::TestDir;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use kbf_outputs::CHUNK_BYTES;

    use super::*;

    /// A fresh scratch directory for one unit test, removed when the test ends.
    fn scratch(name: &str) -> TestDir {
        TestDir::under(std::env::temp_dir(), &format!("kbf-task-outputs-{name}"))
    }

    /// A work directory `work` and log files `out` and `err` under a scratch directory.
    /// The directory is returned too: it is removed when the caller drops it.
    fn rig(name: &str) -> (TestDir, PathBuf, PathBuf, PathBuf) {
        let dir = scratch(name);
        std::fs::create_dir(dir.join("work")).expect("work");
        std::fs::write(dir.join("out"), b"to stdout").expect("out");
        std::fs::write(dir.join("err"), b"to stderr").expect("err");
        let (work, out, err) = (dir.join("work"), dir.join("out"), dir.join("err"));
        (dir, work, out, err)
    }

    async fn gather(
        work: &Path,
        out: &Path,
        err: &Path,
        paths: &[&str],
        limits: OutputLimits,
        cap: u64,
    ) -> Result<Outputs, OutputsError> {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
        collect_outputs(work, "", &paths, out, err, limits, cap).await
    }

    fn text(outputs: &Outputs, digest: &Option<Digest>) -> Vec<u8> {
        let digest = digest.as_ref().expect("a digest");
        outputs
            .blobs
            .read(digest)
            .expect("read")
            .expect("recorded")
            .into_owned()
    }

    /// Catches an output file recorded under another digest than its bytes', or lost, and an
    /// executable bit that is dropped or set on a plain file.
    #[tokio::test]
    async fn files_are_recorded_with_their_executable_bit() {
        let (_dir, work, out, err) = rig("files");
        std::fs::write(work.join("plain"), b"plain bytes").expect("plain");
        std::fs::write(work.join("tool"), b"tool bytes\n").expect("tool");
        std::fs::set_permissions(work.join("tool"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let limits = OutputLimits::default();
        let got = gather(&work, &out, &err, &["plain", "tool"], limits, 1 << 20)
            .await
            .expect("collect");
        let files = &got.result.output_files;
        assert_eq!(files.len(), 2);
        assert_eq!(
            (files[0].path.as_str(), files[0].is_executable),
            ("plain", false)
        );
        assert_eq!(
            (files[1].path.as_str(), files[1].is_executable),
            ("tool", true)
        );
        assert_eq!(files[0].digest, Some(digest_of(b"plain bytes")));
        assert_eq!(text(&got, &files[0].digest), b"plain bytes");
        assert_eq!(text(&got, &files[1].digest), b"tool bytes\n");
    }

    /// Catches stdout or stderr left out of the result or the blobs, or swapped.
    #[tokio::test]
    async fn stdout_and_stderr_are_hashed_and_recorded() {
        let (_dir, work, out, err) = rig("stdio");
        let got = gather(&work, &out, &err, &[], OutputLimits::default(), 1 << 20)
            .await
            .expect("collect");
        assert_eq!(got.result.stdout_digest, Some(digest_of(b"to stdout")));
        assert_eq!(got.result.stderr_digest, Some(digest_of(b"to stderr")));
        assert_eq!(text(&got, &got.result.stdout_digest), b"to stdout");
        assert_eq!(text(&got, &got.result.stderr_digest), b"to stderr");
        assert_eq!(got.blobs.len(), 2);
        assert!(!got.blobs.is_empty());
        assert_eq!(got.blobs.total_bytes(), 18);
    }

    /// Catches an output directory recorded without its Tree blob, or with a Tree that does not
    /// decode to the directory's entries.
    #[tokio::test]
    async fn a_directory_is_recorded_as_a_tree() {
        use prost::Message;
        let (_dir, work, out, err) = rig("tree");
        std::fs::create_dir_all(work.join("d/sub")).expect("dirs");
        std::fs::write(work.join("d/a"), b"a").expect("a");
        std::fs::write(work.join("d/sub/b"), b"b").expect("b");
        let got = gather(&work, &out, &err, &["d"], OutputLimits::default(), 1 << 20)
            .await
            .expect("collect");
        let dir = &got.result.output_directories[0];
        assert_eq!(dir.path, "d");
        let tree = kbf_proto::reapi::Tree::decode(&*text(&got, &dir.tree_digest)).expect("tree");
        let root = tree.root.expect("root");
        assert_eq!(root.files.len(), 1);
        assert_eq!(root.files[0].name, "a");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].files[0].name, "b");
        assert!(
            got.blobs
                .contains(dir.root_directory_digest.as_ref().expect("root digest"))
        );
        assert!(got.blobs.contains(&digest_of(b"a")));
        assert!(got.blobs.contains(&digest_of(b"b")));
    }

    /// Catches a symlink followed or recorded as a file, and a missing output recorded at all.
    #[tokio::test]
    async fn a_symlink_is_kept_and_a_missing_output_is_left_out() {
        let (_dir, work, out, err) = rig("links");
        symlink("elsewhere/target", work.join("link")).expect("link");
        let got = gather(
            &work,
            &out,
            &err,
            &["link", "absent"],
            OutputLimits::default(),
            1 << 20,
        )
        .await
        .expect("collect");
        assert!(got.result.output_files.is_empty());
        assert!(got.result.output_directories.is_empty());
        assert_eq!(got.result.output_symlinks.len(), 1);
        assert_eq!(got.result.output_symlinks[0].path, "link");
        assert_eq!(got.result.output_symlinks[0].target, "elsewhere/target");
    }

    /// Catches a file larger than one chunk read back wrong (from the wrong offset or short), and
    /// two outputs with the same bytes recorded twice.
    #[tokio::test]
    async fn a_large_file_is_read_back_and_equal_blobs_are_recorded_once() {
        let (_dir, work, out, err) = rig("large");
        let large: Vec<u8> = (0..(CHUNK_BYTES + 5)).map(|i| (i % 251) as u8).collect();
        std::fs::write(work.join("big"), &large).expect("big");
        std::fs::write(work.join("copy"), &large).expect("copy");
        let got = gather(
            &work,
            &out,
            &err,
            &["big", "copy"],
            OutputLimits::default(),
            8 << 20,
        )
        .await
        .expect("collect");
        let files = &got.result.output_files;
        assert_eq!(files[0].digest, files[1].digest);
        assert_eq!(text(&got, &files[0].digest), large);
        // The two files and the two logs hold three distinct blobs.
        assert_eq!(got.blobs.len(), 3);
        assert_eq!(got.blobs.digests().count(), 3);
        assert!(
            got.blobs
                .read(&digest_of(b"unrecorded"))
                .expect("read")
                .is_none()
        );
        assert!(!got.blobs.contains(&digest_of(b"unrecorded")));
    }

    /// Catches outputs past the pack's cap being accepted, and a bound counted on an equal blob a
    /// second time.
    #[tokio::test]
    async fn blobs_past_the_cap_fail() {
        let (_dir, work, out, err) = rig("cap");
        std::fs::write(work.join("x"), vec![1u8; 100]).expect("x");
        std::fs::write(work.join("y"), vec![1u8; 100]).expect("y");
        let limits = OutputLimits::default();
        // Equal blobs count once: 100 + 9 + 9 fits in 118.
        gather(&work, &out, &err, &["x", "y"], limits, 118)
            .await
            .expect("fits");
        let why = gather(&work, &out, &err, &["x", "y"], limits, 117)
            .await
            .expect_err("one byte over");
        assert!(matches!(why, OutputsError::Store { .. }), "{why}");
        assert!(why.to_string().contains("pass the 117 bytes"), "{why}");
        assert!(why.to_string().contains("output-too-large"), "{why}");
    }

    /// Catches the output limits not reaching the collection: stdout past
    /// `--output-max-stdio-bytes` must fail.
    #[tokio::test]
    async fn the_output_limits_apply() {
        let (_dir, work, out, err) = rig("limits");
        let limits = OutputLimits {
            max_stdio_bytes: 4,
            ..OutputLimits::default()
        };
        let why = gather(&work, &out, &err, &[], limits, 1 << 20)
            .await
            .expect_err("stdout is over");
        assert!(matches!(why, OutputsError::Limit { .. }), "{why}");
    }

    /// Catches a recorder that fails on a poisoned lock instead of recovering the blobs.
    #[test]
    fn a_poisoned_recorder_still_records() {
        let recorder = Recorder::new(10);
        let held = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = recorder.blobs.lock().expect("lock");
            panic!("poison");
        }));
        assert!(held.is_err());
        recorder
            .record(&digest_of(b"ab"), || Blob::Bytes(b"ab".to_vec()))
            .expect("record");
        assert_eq!(recorder.in_flight(), IN_FLIGHT);
    }
}

#[cfg(test)]
#[path = "outputs_reader_tests.rs"]
mod reader_tests;
