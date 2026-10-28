//! Fixtures of the adoption tests: segments as a storage member builds them, the keys of its
//! three layouts, a rig with some of them stored, and the runs of an adopter over it.

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{KeyPrefix, ObjectKey, ObjectStore};
use kbf_pack::{
    AdoptConfig, AdoptMode, AdoptReport, AdoptRun, Adopter, Blob, Fetch, Problem, ProblemKind,
};
use kbf_pack_index::{NewPack, PackIndex};
use kbf_segments::{MAX_SEGMENT_BYTES, SegmentWriter};

use crate::support::{FaultyStore, Rig, blob};

/// A segment holding `blobs`, as a storage member's flush makes it.
pub fn segment(blobs: &[Blob]) -> Bytes {
    let mut writer = SegmentWriter::new(MAX_SEGMENT_BYTES);
    for blob in blobs {
        writer.push(&blob.bytes).expect("the blob fits");
    }
    Bytes::from(writer.finish())
}

/// `segment(blobs)` with the first byte of its first record changed: a footer that is valid
/// and says what the bytes no longer are.
pub fn tampered(blobs: &[Blob]) -> Bytes {
    let mut bytes = segment(blobs).to_vec();
    bytes[0] ^= 0xff;
    Bytes::from(bytes)
}

/// The key of a storage node's segment.
pub fn node_key(member: u64, boot: u64, seq: u64) -> String {
    format!("kbf/cas/{member:016x}/{boot:016x}/{seq:016x}")
}

/// The key of a segment of a metadata log's epoch.
pub fn epoch_key(epoch: u64, seq: u64) -> String {
    format!("kbf/cas/{epoch:016x}/{seq:016x}")
}

/// The key of a segment of the epoch of another log.
pub fn log_key(log: u64, epoch: u64, seq: u64) -> String {
    format!("kbf/{log:016x}/cas/{epoch:016x}/{seq:016x}")
}

pub async fn put(rig: &Rig, key: &str, bytes: Bytes) {
    let key = ObjectKey::new(key).unwrap();
    rig.store.put_new(&key, bytes, None).await.unwrap();
}

/// Five blobs in three segments, one under each key layout. The keys are returned in the order
/// they were made, which is not the order of a listing: the log's sorts first, then the node's.
pub async fn three(rig: &Rig) -> (Vec<String>, Vec<Blob>) {
    let names = ["alpha", "bravo", "charlie", "delta", "echo"];
    let blobs: Vec<Blob> = names.iter().map(|name| blob(name)).collect();
    let keys = vec![node_key(1, 2, 3), epoch_key(4, 5), log_key(6, 7, 8)];
    put(rig, &keys[0], segment(&blobs[0..2])).await;
    put(rig, &keys[1], segment(&blobs[2..3])).await;
    put(rig, &keys[2], segment(&blobs[3..5])).await;
    (keys, blobs)
}

/// Ten blobs in five segments of one storage node, the keys in the order of a listing.
pub async fn five(rig: &Rig) -> (Vec<String>, Vec<Blob>) {
    let blobs: Vec<Blob> = (0..10).map(|i| blob(&format!("blob-{i}"))).collect();
    let mut keys = Vec::new();
    for (seq, pair) in blobs.chunks(2).enumerate() {
        let key = node_key(1, 1, seq as u64);
        put(rig, &key, segment(pair)).await;
        keys.push(key);
    }
    (keys, blobs)
}

pub fn config(mode: AdoptMode) -> AdoptConfig {
    AdoptConfig::new(mode, KeyPrefix::new("kbf/").unwrap())
}

pub fn make(rig: &Rig, config: AdoptConfig) -> Adopter<FaultyStore> {
    let index: Arc<dyn PackIndex> = rig.index.clone();
    Adopter::new(Arc::clone(&rig.store), index, config)
}

/// A checkpoint saver that keeps nothing.
pub fn ignore(_: &AdoptReport) -> io::Result<()> {
    Ok(())
}

/// A checkpoint saver whose disk is full.
pub fn deny(_: &AdoptReport) -> io::Result<()> {
    Err(io::Error::other("disk full"))
}

/// Runs `adopter` from the start and drops its checkpoints.
pub async fn go(adopter: &Adopter<FaultyStore>) -> AdoptRun {
    adopter.run(None, &mut ignore).await
}

/// Runs `adopter` from `resume`, keeping every checkpoint it saves.
pub async fn saving(
    adopter: &Adopter<FaultyStore>,
    resume: Option<AdoptReport>,
) -> (AdoptRun, Vec<AdoptReport>) {
    let mut saved = Vec::new();
    let mut keep = |report: &AdoptReport| -> io::Result<()> {
        saved.push(report.clone());
        Ok(())
    };
    let run = adopter.run(resume, &mut keep).await;
    (run, saved)
}

pub fn found(blobs: &[Blob]) -> Vec<Fetch> {
    blobs
        .iter()
        .map(|blob| Fetch::Found(blob.bytes.clone()))
        .collect()
}

pub fn problem(key: &str, kind: ProblemKind) -> Problem {
    Problem {
        key: key.to_owned(),
        kind,
    }
}

pub fn writer_of(rig: &Rig, key: &str) -> String {
    rig.index.pack(key).unwrap().unwrap().writer
}

/// A pack of the index with no blobs under `key`, as another writer recorded it.
pub fn foreign_record(key: &str) -> NewPack {
    NewPack {
        key: key.to_owned(),
        writer: "someone-else".to_owned(),
        size: 1 << 20,
        blobs: Vec::new(),
    }
}

/// The counters of a report that most tests compare, in this order: objects, segments, blobs,
/// new packs, known packs, new rows, existing rows.
pub fn tally(report: &AdoptReport) -> [u64; 7] {
    [
        report.objects,
        report.segments,
        report.blobs,
        report.packs_new,
        report.packs_known,
        report.rows_new,
        report.rows_existing,
    ]
}
