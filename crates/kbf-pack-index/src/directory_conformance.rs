//! The conformance suite for the Directory rows ([`PackIndex::directories`] and
//! [`PackIndex::record_directories`]), D1 to D7, over the same [`PackBackend`] factory the pack
//! index's suite uses, so the in-memory index and the Postgres one run the same clauses. Every
//! assertion message starts with its clause and says what a wrong index would do. No clause
//! reads the wall clock, and waits are joins.

use std::sync::Arc;
use std::thread;

use sha2::{Digest as _, Sha256};

use crate::conformance::{PackBackend, same, yes};
use crate::directory::{MAX_DIRECTORY_BYTES, NewDirectory};
use crate::{BlobDigest, MAX_BATCH, PackIndex, PackIndexError};
use kbf_state::ManualClock;

/// Runs D1 to D7 against `backend`.
pub fn run_all<B: PackBackend>(backend: &B) {
    d1_a_recorded_directory_is_read_back(backend);
    d2_a_record_repeats(backend);
    d3_bytes_that_are_not_the_digest_are_refused(backend);
    d4_bodies_and_batches_are_bounded(backend);
    d5_an_empty_call_is_an_empty_answer(backend);
    d6_concurrent_records_agree(backend);
    d7_directory_rows_are_not_blob_rows(backend);
}

fn open<B: PackBackend>(backend: &B) -> B::Index {
    backend.open_index(Arc::new(ManualClock::default()))
}

/// A Directory whose body is `body`, with the digest of those bytes.
#[must_use]
pub fn directory(body: &[u8]) -> NewDirectory {
    NewDirectory {
        digest: BlobDigest {
            hash: Sha256::digest(body).into(),
            size: u64::try_from(body.len()).unwrap_or(u64::MAX),
        },
        body: body.to_vec(),
    }
}

fn read(index: &dyn PackIndex, digests: &[BlobDigest]) -> Vec<Option<Vec<u8>>> {
    index.directories(digests).expect("read")
}

fn record(index: &dyn PackIndex, directories: &[NewDirectory]) -> usize {
    index.record_directories(directories).expect("record")
}

/// D1: a recorded Directory is read back as its bytes, in the order asked, a digest asked twice
/// answers twice, and a digest never recorded (or one of the same hash and another size) has no
/// row.
pub fn d1_a_recorded_directory_is_read_back<B: PackBackend>(backend: &B) {
    let index = open(backend);
    let (one, two) = (directory(b"dir-one"), directory(b"dir-two"));
    let never = directory(b"never recorded");
    same(
        "D1 a record of two new Directories did not insert two rows",
        record(&index, &[one.clone(), two.clone()]),
        2,
    );
    let mut other_size = one.digest;
    other_size.size += 1;
    same(
        "D1 a read did not answer each digest with its bytes, in the order asked",
        read(
            &index,
            &[two.digest, never.digest, one.digest, other_size, two.digest],
        ),
        vec![
            Some(two.body.clone()),
            None,
            Some(one.body.clone()),
            None,
            Some(two.body),
        ],
    );
}

/// D2: a Directory recorded again inserts nothing and keeps its bytes, and a digest named twice
/// in one call is one row.
pub fn d2_a_record_repeats<B: PackBackend>(backend: &B) {
    let index = open(backend);
    let one = directory(b"dir-one");
    same(
        "D2 a digest named twice in one call was not one row",
        record(&index, &[one.clone(), one.clone()]),
        1,
    );
    same(
        "D2 a second record of a Directory inserted a row",
        record(&index, std::slice::from_ref(&one)),
        0,
    );
    let two = directory(b"dir-two");
    same(
        "D2 a call with one recorded and one new Directory did not insert only the new one",
        record(&index, &[one.clone(), two]),
        1,
    );
    same(
        "D2 a repeated record changed the row's bytes",
        read(&index, &[one.digest]),
        vec![Some(one.body)],
    );
}

/// D3: a body whose SHA-256 or length is not its digest is refused, and the whole call writes
/// nothing, its good Directories included.
pub fn d3_bytes_that_are_not_the_digest_are_refused<B: PackBackend>(backend: &B) {
    let index = open(backend);
    let good = directory(b"good");
    let mut wrong_hash = directory(b"bad");
    wrong_hash.digest.hash = directory(b"else").digest.hash;
    let mut wrong_size = directory(b"bad");
    wrong_size.digest.size = 2;
    for bad in [wrong_hash, wrong_size] {
        let refused = index.record_directories(&[good.clone(), bad.clone()]);
        same(
            "D3 a Directory whose bytes are not its digest was not refused as invalid",
            refused,
            Err(PackIndexError::Invalid(
                "a Directory's bytes are not its digest",
            )),
        );
        same(
            "D3 a refused call wrote a row",
            read(&index, &[good.digest, bad.digest]),
            vec![None, None],
        );
    }
}

/// D4: a body longer than [`MAX_DIRECTORY_BYTES`] is refused and one of exactly that length is
/// taken; a call of more than [`MAX_BATCH`] digests or Directories is refused and one of exactly
/// that many is taken.
pub fn d4_bodies_and_batches_are_bounded<B: PackBackend>(backend: &B) {
    let index = open(backend);
    let longest = directory(&vec![7_u8; MAX_DIRECTORY_BYTES]);
    let over = directory(&vec![7_u8; MAX_DIRECTORY_BYTES + 1]);
    same(
        "D4 a Directory one byte over the bound was not refused",
        index.record_directories(&[over]),
        Err(PackIndexError::Invalid(
            "a Directory is longer than a directory row holds",
        )),
    );
    same(
        "D4 a Directory of exactly the bound was not taken",
        record(&index, &[longest]),
        1,
    );
    let batch: Vec<NewDirectory> = (0..=MAX_BATCH)
        .map(|n| directory(format!("dir-{n}").as_bytes()))
        .collect();
    let too_many = MAX_BATCH + 1;
    let refused = Err(PackIndexError::TooMany {
        given: too_many,
        max: MAX_BATCH,
    });
    same(
        "D4 a record of more than the batch bound was not refused",
        index.record_directories(&batch).map(|_| 0),
        refused.clone(),
    );
    let digests: Vec<BlobDigest> = batch.iter().map(|d| d.digest).collect();
    same(
        "D4 a read of more than the batch bound was not refused",
        index.directories(&digests).map(|_| 0),
        refused.clone(),
    );
    same(
        "D4 a record of exactly the batch bound did not insert every row",
        record(&index, &batch[..MAX_BATCH]),
        MAX_BATCH,
    );
    let found = read(&index, &digests[..MAX_BATCH]);
    yes(
        "D4 a read of exactly the batch bound did not find every row",
        found.iter().all(Option::is_some),
    );
}

/// D5: a call that names nothing answers nothing and writes nothing.
pub fn d5_an_empty_call_is_an_empty_answer<B: PackBackend>(backend: &B) {
    let index = open(backend);
    same("D5 an empty record inserted a row", record(&index, &[]), 0);
    same(
        "D5 an empty read answered something",
        read(&index, &[]),
        Vec::new(),
    );
}

/// D6: records of the same Directories from two handles at once all succeed, insert each row
/// once between them, and each handle reads what the other wrote.
pub fn d6_concurrent_records_agree<B: PackBackend>(backend: &B) {
    let first = open(backend);
    let second = backend.reconnect(&first);
    let batch: Vec<NewDirectory> = (0..64)
        .map(|n| directory(format!("shared-{n}").as_bytes()))
        .collect();
    let mut reversed = batch.clone();
    reversed.reverse();
    let inserted = thread::scope(|scope| {
        let one = scope.spawn(|| record(&first, &batch));
        let two = scope.spawn(|| record(&second, &reversed));
        one.join().expect("first record") + two.join().expect("second record")
    });
    same(
        "D6 two records of the same rows did not insert each row exactly once between them",
        inserted,
        64,
    );
    let digest = directory(b"shared-9").digest;
    same(
        "D6 a second handle did not read a row the first wrote",
        read(&second, &[digest]),
        vec![Some(b"shared-9".to_vec())],
    );
}

/// D7: a Directory row is not a blob row: recording one does not make the blob locatable or
/// present, so a Directory whose blob is not in a pack is still missing.
pub fn d7_directory_rows_are_not_blob_rows<B: PackBackend>(backend: &B) {
    let index = open(backend);
    let one = directory(b"dir-one");
    record(&index, std::slice::from_ref(&one));
    same(
        "D7 a Directory row made its blob locatable",
        index.locate(&[one.digest]).expect("locate"),
        vec![None],
    );
    same(
        "D7 a Directory row made its blob present",
        index.find_missing(&[one.digest]).expect("missing"),
        vec![one.digest],
    );
}
