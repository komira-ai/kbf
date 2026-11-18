//! The walk of an input tree (`collect_closure`) over a frontend with no storage node: each
//! level asks the Directory rows of the pack index first, reads only the Directories without a
//! row from their packs, and records those, so a second walk of the same tree reads no
//! Directory from the object store. The rows never decide the answer: a walk whose rows cannot
//! be read or written, or whose index keeps none, answers as a walk of the blobs does.

use bytes::Bytes;
use kbf_front::prepare_run::{
    ClosureError, ClosureLimits, DetachedWrites, InputClosure, PutDirPrep,
    collect_closure_detaching,
};
use kbf_front::{PackKeyMint, PackSettings};
use kbf_pack_index::directory::{MAX_DIRECTORY_BYTES, NewDirectory};
use kbf_pack_index::{MAX_BATCH, PackIndex as _};
use kbf_proto::reapi::{self, Directory, DirectoryNode, FileNode};
use kbf_types::Digest;
use prost::Message as _;

use crate::bytestream::{pieces, write};
use crate::cas::{item, update};
use crate::rig::{Rig, blob_of, index_form};

fn wire(digest: &Digest) -> reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// The Directory that names `files` and `dirs` (each by a name and a digest), and its blob.
fn dir(files: &[(&str, Digest)], dirs: &[(&str, Digest)]) -> (Digest, Bytes) {
    let message = Directory {
        files: files
            .iter()
            .map(|(name, digest)| FileNode {
                name: (*name).to_owned(),
                digest: Some(wire(digest)),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .map(|(name, digest)| DirectoryNode {
                name: (*name).to_owned(),
                digest: Some(wire(digest)),
            })
            .collect(),
        ..Directory::default()
    };
    blob_of(&message.encode_to_vec())
}

/// The Action that names `command` and `root`, and its blob.
fn action_of(command: &Digest, root: &Digest) -> (Digest, Bytes) {
    let message = reapi::Action {
        command_digest: Some(wire(command)),
        input_root_digest: Some(wire(root)),
        ..reapi::Action::default()
    };
    blob_of(&message.encode_to_vec())
}

/// Uploads `blobs` with one BatchUpdateBlobs and checks each was taken.
async fn upload(rig: &Rig, blobs: &[(Digest, Bytes)]) {
    let requests = blobs.iter().map(|(d, b)| item(d, b)).collect();
    let codes = update(rig, requests).await.expect("uploaded");
    assert!(codes.iter().all(|&c| c == 0), "{codes:?}");
}

/// A tree of the root, a subdirectory `sub` holding the file `f`, and the empty Directory
/// `empty`; the Action over it and its Command are uploaded with it.
struct Tree {
    action: Digest,
    root: (Digest, Bytes),
    sub: (Digest, Bytes),
    empty: Digest,
}

async fn uploaded_tree(rig: &Rig) -> Tree {
    let file = blob_of(b"file f");
    let command = blob_of(b"the command");
    let sub = dir(&[("f", file.0)], &[]);
    let empty = dir(&[], &[]);
    let root = dir(&[], &[("sub", sub.0), ("empty", empty.0)]);
    let action = action_of(&command.0, &root.0);
    upload(
        rig,
        &[file, command, sub.clone(), root.clone(), action.clone()],
    )
    .await;
    Tree {
        action: action.0,
        root,
        sub,
        empty: empty.0,
    }
}

/// The closure of `action`, once the writes the walk left running have ended.
async fn walk(rig: &Rig, action: &Digest) -> Result<InputClosure, ClosureError> {
    let writes = DetachedWrites::new(8);
    let closure =
        collect_closure_detaching(&rig.backend, action, ClosureLimits::DEFAULT, &writes).await;
    writes.settled().await;
    closure
}

/// The Directory rows the index holds for `digests`, in order.
fn rows(rig: &Rig, digests: &[Digest]) -> Vec<Option<Vec<u8>>> {
    let asked: Vec<_> = digests.iter().map(index_form).collect();
    rig.index.inner.directories(&asked).expect("rows read")
}

fn row_of(directory: &(Digest, Bytes)) -> NewDirectory {
    NewDirectory {
        digest: index_form(&directory.0),
        body: directory.1.to_vec(),
    }
}

/// Asserts that a first walk records every Directory it read from a pack as a row (the empty
/// Directory, which it never reads, gets none), and that a second walk of the same tree gives
/// the same closure while reading two fewer ranges of the object store: the two Directories
/// come from their rows.
#[tokio::test]
async fn a_walk_records_what_it_read_and_the_next_walk_reads_the_rows() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;

    let before = rig.store.ranges().len();
    let first = walk(&rig, &tree.action).await.expect("the first walk");
    let first_reads = rig.store.ranges().len() - before;
    let recorded = rows(&rig, &[tree.root.0, tree.sub.0, tree.empty]);
    let between = rig.store.ranges().len();
    let second = walk(&rig, &tree.action).await.expect("the second walk");
    let second_reads = rig.store.ranges().len() - between;

    assert_eq!(
        recorded,
        vec![Some(tree.root.1.to_vec()), Some(tree.sub.1.to_vec()), None]
    );
    assert_eq!(first, second);
    assert_eq!(
        first_reads - second_reads,
        2,
        "{first_reads} {second_reads}"
    );
}

/// Asserts that a Directory with a row is taken from the row: the walk succeeds over a
/// subdirectory whose blob was never uploaded, and the closure holds the file inside it.
#[tokio::test]
async fn a_directory_with_a_row_is_not_read_from_the_object_store() {
    let rig = Rig::new().await;
    let file = blob_of(b"file f");
    let command = blob_of(b"the command");
    let only_a_row = dir(&[("f", file.0)], &[]);
    let root = dir(&[], &[("sub", only_a_row.0)]);
    let action = action_of(&command.0, &root.0);
    upload(&rig, &[file.clone(), command, root, action.clone()]).await;
    rig.index
        .inner
        .record_directories(&[row_of(&only_a_row)])
        .expect("the row");

    let closure = walk(&rig, &action.0).await.expect("walked");

    assert!(closure.inputs().any(|d| *d == only_a_row.0));
    assert!(closure.inputs().any(|d| *d == file.0));
}

/// Asserts that when the rows cannot be read the walk reads the level from the blobs and gives
/// the closure a walk with rows gives, and still records what it read.
#[tokio::test]
async fn rows_that_cannot_be_read_leave_the_walk_to_the_blobs() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;

    rig.index.fail_directories(2);
    let without = walk(&rig, &tree.action).await.expect("walked from blobs");
    let with = walk(&rig, &tree.action).await.expect("walked from rows");

    assert_eq!(without, with);
    assert_eq!(
        rows(&rig, &[tree.root.0, tree.sub.0]),
        vec![Some(tree.root.1.to_vec()), Some(tree.sub.1.to_vec())]
    );
}

/// Asserts that a record that fails (the one record of a walk) leaves the walk its answer and
/// writes no row; the next walk records the Directories.
#[tokio::test]
async fn a_record_that_fails_leaves_the_walk_its_answer() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;

    rig.index.fail_record_directories(1);
    let first = walk(&rig, &tree.action).await.expect("walked");
    let after_failure = rows(&rig, &[tree.root.0, tree.sub.0]);
    let second = walk(&rig, &tree.action).await.expect("walked again");

    assert_eq!(first, second);
    assert_eq!(after_failure, vec![None, None]);
    assert_eq!(
        rows(&rig, &[tree.root.0, tree.sub.0]),
        vec![Some(tree.root.1.to_vec()), Some(tree.sub.1.to_vec())]
    );
}

/// Asserts that an index that keeps no Directory rows leaves every walk to the blobs, with the
/// same closure, and that nothing is recorded.
#[tokio::test]
async fn an_index_that_keeps_no_rows_leaves_the_walk_to_the_blobs() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;
    rig.index.keep_no_directories();

    let first = walk(&rig, &tree.action).await.expect("walked");
    let second = walk(&rig, &tree.action).await.expect("walked again");

    assert_eq!(first, second);
    assert_eq!(rows(&rig, &[tree.root.0, tree.sub.0]), vec![None, None]);
}

/// Asserts that a row whose bytes are not a Directory is refused as the blob would be: the
/// walk is `Invalid` for "a Directory", naming the digest.
#[tokio::test]
async fn a_row_that_is_not_a_directory_is_refused_as_its_blob_would_be() {
    let rig = Rig::new().await;
    let garbage = blob_of(&[0xff, 0xff, 0xff]);
    let command = blob_of(b"the command");
    let root = dir(&[], &[("bad", garbage.0)]);
    let action = action_of(&command.0, &root.0);
    upload(&rig, &[command, root, action.clone()]).await;
    rig.index
        .inner
        .record_directories(&[row_of(&garbage)])
        .expect("the row");

    let refused = walk(&rig, &action.0).await;

    assert!(
        matches!(
            refused,
            Err(ClosureError::Invalid { what: "a Directory", digest, .. }) if digest == garbage.0
        ),
        "{refused:?}"
    );
}

/// Asserts that a level of more Directories than one call of the index takes is asked and
/// recorded in calls of at most a batch: every Directory of the level gets its row, and a
/// second walk gives the same closure.
#[tokio::test]
async fn a_level_of_more_than_a_batch_is_asked_and_recorded_in_batches() {
    // The root names a thousand and one subdirectories: a blob of about 80 KB.
    let rig = Rig::with(PackSettings::default()).await;
    let command = blob_of(b"the command");
    let mut subs = Vec::new();
    let mut blobs = vec![command.clone()];
    for n in 0..=MAX_BATCH {
        let file = blob_of(format!("file {n}").as_bytes());
        let sub = dir(&[("f", file.0)], &[]);
        blobs.push(file);
        blobs.push(sub.clone());
        subs.push(sub);
    }
    let names: Vec<String> = (0..subs.len()).map(|n| format!("d{n:05}")).collect();
    let named: Vec<(&str, Digest)> = names
        .iter()
        .zip(&subs)
        .map(|(name, sub)| (name.as_str(), sub.0))
        .collect();
    let root = dir(&[], &named);
    let action = action_of(&command.0, &root.0);
    blobs.push(root);
    blobs.push(action.clone());
    for chunk in blobs.chunks(500) {
        upload(&rig, chunk).await;
    }

    let first = walk(&rig, &action.0).await.expect("walked");
    let digests: Vec<Digest> = subs.iter().map(|s| s.0).collect();
    let recorded: Vec<Option<Vec<u8>>> = digests
        .chunks(MAX_BATCH)
        .flat_map(|chunk| rows(&rig, chunk))
        .collect();
    let second = walk(&rig, &action.0).await.expect("walked again");

    assert_eq!(subs.len(), MAX_BATCH + 1);
    assert!(recorded.iter().all(Option::is_some));
    assert_eq!(first, second);
}

/// Asserts that a Directory longer than a row holds is read from its blob on every walk and
/// never recorded, while the root beside it is.
#[tokio::test]
async fn a_directory_over_the_row_bound_is_read_from_its_blob_and_not_recorded() {
    let rig = Rig::with(PackSettings::default()).await;
    let file = blob_of(b"file f");
    let command = blob_of(b"the command");
    let long_name = "n".repeat(MAX_DIRECTORY_BYTES);
    let long = dir(&[(long_name.as_str(), file.0)], &[]);
    assert!(long.1.len() > MAX_DIRECTORY_BYTES);
    let root = dir(&[], &[("long", long.0)]);
    let action = action_of(&command.0, &root.0);
    upload(&rig, &[file, command, root.clone(), action.clone()]).await;
    write(&rig, pieces(&long.0, &long.1, 1 << 20))
        .await
        .expect("the long Directory uploaded");

    let first = walk(&rig, &action.0).await.expect("walked");
    let second = walk(&rig, &action.0).await.expect("walked again");

    assert_eq!(first, second);
    assert_eq!(
        rows(&rig, &[root.0, long.0]),
        vec![Some(root.1.to_vec()), None]
    );
}

const LEASE: &str = "00000000000000010000000000000002";

/// A preparer of put-directory runs over the rig's store, its writes after the walk bounded by
/// `writes`.
fn preparer(rig: &Rig, writes: &DetachedWrites) -> PutDirPrep {
    PutDirPrep::new(
        rig.backend.clone(),
        PackKeyMint::new(b"0123456789abcdef0123456789abcdef").expect("a mint"),
        std::time::Duration::from_secs(600),
        ClosureLimits::DEFAULT,
    )
    .with_detached_writes(writes.clone())
}

/// Prepares the put-directory run of `action`: its kept closure's Directory count.
async fn prepared_directories(prep: &PutDirPrep, action: &Digest) -> usize {
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_040);
    let wall = std::time::Duration::from_secs(7_200);
    let run = prep.prepare_put_dir(LEASE, action, now, wall).await;
    run.expect("prepared").kept.directory_count()
}

/// Asserts that a preparation returns, with the whole closure, while the record of the rows is
/// still waiting inside the index (it had been started, and no row is there yet), and that once
/// the record is let go and has ended the rows of the Directories read are there.
#[tokio::test]
async fn a_preparation_returns_while_the_rows_are_still_being_written() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;
    let writes = DetachedWrites::new(8);
    let prep = preparer(&rig, &writes);
    rig.index.hold_record_directories();

    let kept = prepared_directories(&prep, &tree.action).await;
    rig.index.record_directories_entered().await;
    let while_held = rows(&rig, &[tree.root.0, tree.sub.0]);
    rig.index.release_record_directories();
    writes.settled().await;

    assert_eq!(kept, 3, "the root, `sub` and the empty Directory");
    assert_eq!(while_held, vec![None, None]);
    assert_eq!(
        rows(&rig, &[tree.root.0, tree.sub.0]),
        vec![Some(tree.root.1.to_vec()), Some(tree.sub.1.to_vec())]
    );
    assert_eq!(writes.skipped(), 0);
}

/// Asserts that a record that fails after the preparation returned changes nothing the run was
/// given: the same closure, no row written, and the next preparation records the rows.
#[tokio::test]
async fn a_failed_row_write_does_not_fail_the_preparation() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;
    let writes = DetachedWrites::new(8);
    let prep = preparer(&rig, &writes);

    rig.index.fail_record_directories(1);
    let first = prepared_directories(&prep, &tree.action).await;
    writes.settled().await;
    let after_failure = rows(&rig, &[tree.root.0, tree.sub.0]);
    let second = prepared_directories(&prep, &tree.action).await;
    writes.settled().await;

    assert_eq!((first, second), (3, 3));
    assert_eq!(after_failure, vec![None, None]);
    assert_eq!(
        rows(&rig, &[tree.root.0, tree.sub.0]),
        vec![Some(tree.root.1.to_vec()), Some(tree.sub.1.to_vec())]
    );
}

/// Asserts the bound: with its only permit taken by a record that is waiting, a second
/// preparation of the tree still succeeds, skips its own record and counts the skip; when the
/// first record ends, its rows are the only ones written.
#[tokio::test]
async fn a_write_with_no_permit_free_is_skipped_and_counted() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;
    let writes = DetachedWrites::new(1);
    let prep = preparer(&rig, &writes);
    rig.index.hold_record_directories();

    let first = prepared_directories(&prep, &tree.action).await;
    rig.index.record_directories_entered().await;
    let skipped_before = writes.skipped();
    let second = prepared_directories(&prep, &tree.action).await;
    let skipped_after = writes.skipped();
    rig.index.release_record_directories();
    writes.settled().await;

    assert_eq!((first, second), (3, 3));
    assert_eq!((skipped_before, skipped_after), (0, 1));
    assert_eq!(
        rows(&rig, &[tree.root.0, tree.sub.0]),
        vec![Some(tree.root.1.to_vec()), Some(tree.sub.1.to_vec())]
    );
}

/// Asserts that a bound of no permits skips every write, counted once per preparation, and the
/// preparation still gives its closure.
#[tokio::test]
async fn a_bound_of_no_writes_skips_them_all() {
    let rig = Rig::new().await;
    let tree = uploaded_tree(&rig).await;
    let writes = DetachedWrites::new(0);
    let prep = preparer(&rig, &writes);

    let kept = prepared_directories(&prep, &tree.action).await;
    writes.settled().await;

    assert_eq!(kept, 3);
    assert_eq!(writes.skipped(), 1);
    assert_eq!(rows(&rig, &[tree.root.0, tree.sub.0]), vec![None, None]);
}
