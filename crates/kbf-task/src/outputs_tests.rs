use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::Mutex;

use kbf_outputs::CHUNK_BYTES;
use kbf_task_pack::one_pass::{Entry, TAIL_LEN, Tail, Trailer};
use kbf_testkit::dir::TestDir;
use prost::Message as _;

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

/// The file a spool writes to, readable by the test.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a collection left: its result, and the object it wrote the blobs to.
struct Collected {
    result: ActionResult,
    object: Vec<u8>,
    entries: Vec<Entry>,
}

impl Collected {
    /// How many records of the object hold the blob `digest` names.
    fn records_of(&self, digest: &Digest) -> usize {
        let wanted = crate::digest::from_reapi(digest).expect("a digest");
        self.entries.iter().filter(|e| e.digest == wanted).count()
    }

    /// The bytes of the blob `digest` names, as the object holds them.
    fn text(&self, digest: &Option<Digest>) -> Vec<u8> {
        let wanted = crate::digest::from_reapi(digest.as_ref().expect("a digest")).expect("digest");
        let entry = self
            .entries
            .iter()
            .find(|e| e.digest == wanted)
            .expect("the blob is in the object");
        self.object[entry.range().start as usize..entry.range().end as usize].to_vec()
    }
}

/// Collects `paths` of `work` with the logs `out` and `err` into an object.
async fn gather(
    work: &Path,
    out: &Path,
    err: &Path,
    paths: &[&str],
    limits: OutputLimits,
) -> Result<Collected, OutputsError> {
    let file = Shared::default();
    let spool = Spool::new(Box::new(file.clone()));
    let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
    let result = collect_spooled(work, "", &paths, out, err, limits, &spool).await?;
    spool.finish(b"{}", b"").expect("finished");
    let object = file.0.lock().expect("lock").clone();
    let trailer =
        Trailer::parse(&object[object.len() - TAIL_LEN..], object.len() as u64).expect("trailer");
    let tail = &object[object.len() - usize::try_from(trailer.tail_len()).expect("fits")..];
    let entries = Tail::parse(&trailer, tail).expect("tail").entries;
    Ok(Collected {
        result,
        object,
        entries,
    })
}

/// Catches an output file written under another digest than its bytes', or lost, and an
/// executable bit that is dropped or set on a plain file.
#[tokio::test]
async fn files_are_collected_with_their_executable_bit() {
    let (_dir, work, out, err) = rig("files");
    std::fs::write(work.join("plain"), b"plain bytes").expect("plain");
    std::fs::write(work.join("tool"), b"tool bytes\n").expect("tool");
    std::fs::set_permissions(work.join("tool"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    let got = gather(
        &work,
        &out,
        &err,
        &["plain", "tool"],
        OutputLimits::default(),
    )
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
    assert_eq!(got.text(&files[0].digest), b"plain bytes");
    assert_eq!(got.text(&files[1].digest), b"tool bytes\n");
}

/// Catches stdout or stderr left out of the result or the object, or swapped.
#[tokio::test]
async fn stdout_and_stderr_are_hashed_and_written() {
    let (_dir, work, out, err) = rig("stdio");
    let got = gather(&work, &out, &err, &[], OutputLimits::default())
        .await
        .expect("collect");
    assert_eq!(got.result.stdout_digest, Some(digest_of(b"to stdout")));
    assert_eq!(got.result.stderr_digest, Some(digest_of(b"to stderr")));
    assert_eq!(got.text(&got.result.stdout_digest), b"to stdout");
    assert_eq!(got.text(&got.result.stderr_digest), b"to stderr");
    assert_eq!(got.entries.len(), 2);
}

/// Catches an output directory written without its Tree blob, or with a Tree that does not
/// decode to the directory's entries.
#[tokio::test]
async fn a_directory_is_written_as_a_tree() {
    let (_dir, work, out, err) = rig("tree");
    std::fs::create_dir_all(work.join("d/sub")).expect("dirs");
    std::fs::write(work.join("d/a"), b"a").expect("a");
    std::fs::write(work.join("d/sub/b"), b"b").expect("b");
    let got = gather(&work, &out, &err, &["d"], OutputLimits::default())
        .await
        .expect("collect");
    let dir = &got.result.output_directories[0];
    assert_eq!(dir.path, "d");
    let tree = kbf_proto::reapi::Tree::decode(&*got.text(&dir.tree_digest)).expect("tree");
    let root = tree.root.expect("root");
    assert_eq!(root.files.len(), 1);
    assert_eq!(root.files[0].name, "a");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].files[0].name, "b");
    assert_eq!(
        got.records_of(dir.root_directory_digest.as_ref().expect("root digest")),
        1
    );
    assert_eq!(got.records_of(&digest_of(b"a")), 1);
    assert_eq!(got.records_of(&digest_of(b"b")), 1);
}

/// Catches a symlink followed or written as a file, and a missing output written at all.
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
    )
    .await
    .expect("collect");
    assert!(got.result.output_files.is_empty());
    assert!(got.result.output_directories.is_empty());
    assert_eq!(got.result.output_symlinks.len(), 1);
    assert_eq!(got.result.output_symlinks[0].path, "link");
    assert_eq!(got.result.output_symlinks[0].target, "elsewhere/target");
}

/// Catches a file larger than one chunk written wrong (from the wrong offset or short), and two
/// outputs with the same bytes written twice.
#[tokio::test]
async fn a_large_file_is_written_whole_and_equal_blobs_once() {
    let (_dir, work, out, err) = rig("large");
    let large: Vec<u8> = (0..(CHUNK_BYTES + 5)).map(|i| (i % 251) as u8).collect();
    std::fs::write(work.join("big"), &large).expect("big");
    std::fs::write(work.join("copy"), &large).expect("copy");
    let got = gather(&work, &out, &err, &["big", "copy"], OutputLimits::default())
        .await
        .expect("collect");
    let files = &got.result.output_files;
    assert_eq!(files[0].digest, files[1].digest);
    assert_eq!(got.text(&files[0].digest), large);
    // The two files and the two logs hold three distinct blobs.
    assert_eq!(got.entries.len(), 3);
    assert_eq!(got.records_of(files[0].digest.as_ref().expect("digest")), 1);
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
    let Err(why) = gather(&work, &out, &err, &[], limits).await else {
        panic!("stdout is over");
    };
    assert!(matches!(why, OutputsError::Limit { .. }), "{why}");
}

/// A file handed to the store whole is refused: its bytes go to the object as they are hashed.
#[tokio::test]
async fn a_file_handed_over_whole_is_refused() {
    let (_dir, _work, out, _err) = rig("handed");
    let store = Spooler {
        spool: Spool::new(Box::new(Shared::default())),
    };
    assert_eq!(store.in_flight(), IN_FLIGHT);
    let file = std::fs::File::open(out).expect("open");
    let why = store
        .put_file(file, digest_of(b"x"))
        .await
        .expect_err("refused");
    assert!(why.to_string().contains("handed over"), "{why}");
}
