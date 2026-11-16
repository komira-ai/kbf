//! The spool: blobs from memory and teed files in one object, and what it says when it cannot.

use std::sync::{Arc, Mutex};

use kbf_segments::sha256;
use kbf_task_pack::one_pass::{TAIL_LEN, Tail, Trailer};

use prost::Message as _;

use super::*;
use crate::digest::to_reapi;

/// A file the test can read back.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A file that refuses every write.
struct Refusing;

impl Write for Refusing {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }
}

/// The entries of the finished object `bytes`.
fn entries(bytes: &[u8]) -> Vec<kbf_task_pack::one_pass::Entry> {
    let len = bytes.len() as u64;
    let trailer = Trailer::parse(&bytes[bytes.len() - TAIL_LEN..], len).expect("trailer");
    let tail = &bytes[bytes.len() - usize::try_from(trailer.tail_len()).expect("fits")..];
    Tail::parse(&trailer, tail).expect("tail").entries
}

/// A manifest whose standard output is `stdout`.
fn manifest_naming(stdout: kbf_types::Digest) -> TaskManifest {
    TaskManifest {
        lease: "lease-1".to_owned(),
        action: sha256(b"action"),
        exit_code: 0,
        stdout: Some(stdout),
        stderr: None,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: kbf_task_pack::Timing::default(),
        usage: kbf_task_pack::Usage::default(),
        packs: 1,
        large: Vec::new(),
        inputs: None,
        objects: None,
    }
}

/// Opens a tee file on the blocking pool, as the walk does.
async fn open(spool: &Arc<Spool>) -> io::Result<Box<dyn TeeFile>> {
    let spool = Arc::clone(spool);
    tokio::task::spawn_blocking(move || spool.open())
        .await
        .expect("joined")
}

#[tokio::test]
async fn blobs_from_memory_and_a_teed_file_make_one_object() {
    let file = Shared::default();
    let spool = Spool::new(Box::new(file.clone()));
    spool
        .put(&to_reapi(&sha256(b"small")), b"small".to_vec())
        .await
        .expect("put");
    // The same blob again is not written twice.
    spool
        .put(&to_reapi(&sha256(b"small")), b"small".to_vec())
        .await
        .expect("put again");
    let mut tee = open(&spool).await.expect("open");
    tee.write(b"big ").expect("write");
    tee.write(b"file").expect("write");
    tee.end(&to_reapi(&sha256(b"big file"))).expect("end");
    spool.finish(b"{}", b"").expect("finished");
    let bytes = file.0.lock().expect("lock").clone();
    let found = entries(&bytes);
    assert_eq!(found.len(), 2);
    for (blob, first) in [(&b"small"[..], true), (&b"big file"[..], false)] {
        let entry = found
            .iter()
            .find(|e| e.digest == sha256(blob))
            .expect("an entry");
        assert!(entry.verify(&bytes[entry.range().start as usize..entry.range().end as usize]));
        assert_eq!(entry.offset == 68, first, "the memory blob came first");
    }
}

#[tokio::test]
async fn a_blob_put_while_a_file_is_teed_waits_and_lands_after_it() {
    let file = Shared::default();
    let spool = Spool::new(Box::new(file.clone()));
    let mut tee = open(&spool).await.expect("open");
    tee.write(b"aaaa").expect("write");
    let waiting = {
        let spool = Arc::clone(&spool);
        tokio::spawn(async move { spool.put(&to_reapi(&sha256(b"zz")), b"zz".to_vec()).await })
    };
    // `put` cannot take the writer while the file is open: it is still pending when the file's
    // bytes are all in.
    tee.write(b"bbbb").expect("write");
    tee.end(&to_reapi(&sha256(b"aaaabbbb"))).expect("end");
    waiting.await.expect("joined").expect("put");
    spool.finish(b"{}", b"").expect("finished");
    let bytes = file.0.lock().expect("lock").clone();
    // The put waited for the teed file: its bytes are whole, and `zz` is after them.
    let found = entries(&bytes);
    let at = |blob: &[u8]| {
        found
            .iter()
            .find(|e| e.digest == sha256(blob))
            .expect("entry")
            .offset
    };
    assert_eq!(at(b"aaaabbbb"), 68);
    assert_eq!(&bytes[68..76], b"aaaabbbb");
    assert!(at(b"zz") > at(b"aaaabbbb") + 8);
}

#[tokio::test]
async fn what_the_manifest_names_that_the_object_lacks_is_missing() {
    let spool = Spool::new(Box::new(Shared::default()));
    spool
        .put(&to_reapi(&sha256(b"here")), b"here".to_vec())
        .await
        .expect("put");
    let mut manifest = manifest_naming(sha256(b"here"));
    assert_eq!(spool.missing(&manifest).await, None);
    manifest.stdout = Some(sha256(b"gone"));
    assert_eq!(spool.missing(&manifest).await, Some(sha256(b"gone")));
}

#[tokio::test]
async fn a_digest_that_is_not_sha256_text_is_refused_by_put_and_end() {
    let spool = Spool::new(Box::new(Shared::default()));
    let bad = reapi::Digest {
        hash: "NOT-HEX".to_owned(),
        size_bytes: 3,
    };
    assert!(spool.put(&bad, b"abc".to_vec()).await.is_err());
    let mut tee = open(&spool).await.expect("open");
    tee.write(b"abc").expect("write");
    assert!(tee.end(&bad).is_err());
}

#[tokio::test]
async fn a_file_that_refuses_a_write_is_remembered_whichever_step_it_refused() {
    // A refusal while a blob is put.
    let spool = Spool::new(Box::new(Refusing));
    let big = vec![1u8; BUFFER_BYTES + 1];
    let put = spool.put(&to_reapi(&sha256(&big)), big).await;
    assert!(put.is_err());
    assert_eq!(
        spool.write_failure().expect("remembered").kind(),
        io::ErrorKind::StorageFull
    );
    // A refusal while a teed file's bytes are written, and then at the end of the object.
    let spool = Spool::new(Box::new(Refusing));
    let mut tee = open(&spool).await.expect("open");
    assert!(tee.write(&vec![2u8; BUFFER_BYTES + 1]).is_err());
    drop(tee);
    assert!(spool.write_failure().is_some());
    // A refusal while the manifest is written, and while the file is flushed.
    for manifest in [vec![0u8; BUFFER_BYTES + 1], b"{}".to_vec()] {
        let spool = Spool::new(Box::new(Refusing));
        let kept = Arc::clone(&spool.failure);
        assert!(spool.finish(&manifest, b"").is_err());
        assert!(kept.lock().expect("lock").is_some());
    }
}

#[test]
fn a_spool_that_is_still_shared_cannot_be_finished() {
    let spool = Spool::new(Box::new(Shared::default()));
    let other = Arc::clone(&spool);
    let error = spool.finish(b"{}", b"").expect_err("shared");
    assert_eq!(error.to_string(), "the object is still being written");
    drop(other);
}

#[test]
fn a_spool_with_no_failure_reports_none() {
    let spool = Spool::new(Box::new(Shared::default()));
    assert!(spool.write_failure().is_none());
}

/// A spool over a file the test can look at.
fn readable() -> (Arc<Spool>, Shared) {
    let file = Shared::default();
    (Spool::new(Box::new(file.clone())), file)
}

#[tokio::test]
async fn the_file_index_uses_the_kept_trees_and_gives_offsets_in_the_object() {
    use kbf_proto::reapi::{Directory, FileNode, Tree};
    let (spool, _) = readable();
    let dir = Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            digest: Some(to_reapi(&sha256(b"file bytes"))),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let tree = Tree {
        root: Some(dir),
        children: Vec::new(),
    };
    let tree_bytes = tree.encode_to_vec();
    spool
        .put(&to_reapi(&sha256(b"file bytes")), b"file bytes".to_vec())
        .await
        .expect("put");
    spool.keep_tree(&tree_bytes);
    spool.keep_tree(&tree_bytes);
    spool
        .put(&to_reapi(&sha256(&tree_bytes)), tree_bytes.clone())
        .await
        .expect("put");
    let mut manifest = manifest_naming(sha256(b"file bytes"));
    manifest.output_directories = vec![kbf_task_pack::OutputDirectory {
        path: "d".to_owned(),
        tree: sha256(&tree_bytes),
    }];
    let json = spool.files(&manifest).await.expect("indexed");
    let json = String::from_utf8(json).expect("utf-8");
    assert!(
        json.contains(r#"{"path":"d/f","size":10,"exec":false"#),
        "{json}"
    );
    assert!(json.contains(r#""offset":68}"#), "{json}");
    assert!(json.contains(r#""dirs":["d"]"#), "{json}");
}

#[tokio::test]
async fn a_tree_that_is_not_a_record_not_kept_or_not_a_tree_fails_the_file_index() {
    let (spool, _) = readable();
    let mut manifest = manifest_naming(sha256(b"x"));
    manifest.output_directories = vec![kbf_task_pack::OutputDirectory {
        path: "d".to_owned(),
        tree: sha256(b"never written"),
    }];
    let why = spool.files(&manifest).await.expect_err("not a record");
    assert!(why.to_string().contains("is not a record"), "{why}");

    // A record that is not a Tree.
    let (spool, _) = readable();
    spool
        .put(
            &to_reapi(&sha256(b"\xff\xff junk")),
            b"\xff\xff junk".to_vec(),
        )
        .await
        .expect("put");
    spool.keep_tree(b"\xff\xff junk");
    manifest.output_directories[0].tree = sha256(b"\xff\xff junk");
    assert!(spool.files(&manifest).await.is_err());

    // A record the collector did not keep as a Tree.
    let spool = Spool::new(Box::new(Shared::default()));
    let tree = sha256(b"tree");
    spool
        .put(&to_reapi(&tree), b"tree".to_vec())
        .await
        .expect("put");
    manifest.output_directories[0].tree = tree;
    let why = spool.files(&manifest).await.expect_err("not kept");
    assert!(why.to_string().contains("was not kept"), "{why}");

    // A file that refuses the flush.
    let spool = Spool::new(Box::new(Refusing));
    assert!(spool.files(&manifest).await.is_err());
}
