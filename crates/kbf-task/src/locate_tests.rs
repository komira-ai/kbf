//! Reading inputs where `/locate` points: the table, the fallbacks, the counts, and the HTTP
//! pieces against fake servers. Under `fetch::tests`, so the nextest hang guard covers them.

use std::sync::Arc;

use crate::fake_http::{Answer, Fake};

use super::*;

const GRANT: &str = "grant-1";

/// The listener: [`Memory`] shared with the test, which reads its counters afterwards.
struct Shared(Arc<Memory>);

impl BlobSource for Shared {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.0.get(digest)
    }
}

/// The store: objects by key, a record of every read, keys that refuse, and keys that answer one
/// byte more than asked.
#[derive(Default)]
struct FakeStore {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    reads: Mutex<Vec<(String, Want, String)>>,
    refuse: Mutex<HashSet<String>>,
    overrun: Mutex<HashSet<String>>,
}

impl FakeStore {
    fn put(&self, key: &str, bytes: Vec<u8>) {
        self.objects
            .lock()
            .expect("objects")
            .insert(key.to_owned(), bytes);
    }

    fn reads(&self) -> Vec<(String, Want, String)> {
        self.reads.lock().expect("reads").clone()
    }
}

struct SharedStore(Arc<FakeStore>);

impl ObjectStore for SharedStore {
    fn read(
        &self,
        key: &str,
        want: Want,
        grant: &str,
        out: &mut dyn Write,
    ) -> Result<(), StoreError> {
        let failed = |why: &str| StoreError::Failed(why.to_owned());
        let store = &self.0;
        store
            .reads
            .lock()
            .expect("reads")
            .push((key.to_owned(), want, grant.to_owned()));
        if store.refuse.lock().expect("refuse").contains(key) {
            return Err(failed("403"));
        }
        let objects = store.objects.lock().expect("objects");
        let object = objects.get(key).ok_or_else(|| failed("404"))?;
        let (start, end) = match want {
            Want::Whole { .. } => (0, object.len()),
            Want::Range { offset, length } => {
                let start = usize::try_from(offset).expect("offset").min(object.len());
                let end = usize::try_from(offset + length)
                    .expect("end")
                    .min(object.len());
                (start, end)
            }
        };
        out.write_all(&object[start..end])
            .map_err(|e| failed(&e.to_string()))?;
        if store.overrun.lock().expect("overrun").contains(key) {
            out.write_all(b"x").map_err(|e| failed(&e.to_string()))?;
        }
        Ok(())
    }
}

/// `blobs` laid end to end behind a four-byte header, and where each starts.
fn pack_of(blobs: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
    let mut pack = b"HEAD".to_vec();
    let mut offsets = Vec::new();
    for blob in blobs {
        offsets.push(pack.len() as u64);
        pack.extend_from_slice(blob);
    }
    (pack, offsets)
}

fn pack_line(digest: &Digest, key: &str, offset: u64) -> String {
    pack_line_g(digest, key, offset, 0)
}

fn pack_line_g(digest: &Digest, key: &str, offset: u64, grant: usize) -> String {
    format!(
        r#"{{"digest":"{}","pack":{{"key":"{key}","offset":{offset},"length":{}}},"g":{grant}}}"#,
        label(digest),
        digest.size_bytes
    )
}

fn large_line(digest: &Digest) -> String {
    format!(
        r#"{{"digest":"{}","large":{{"key":"cas/l/{}","size":{}}},"g":0}}"#,
        label(digest),
        digest.hash,
        digest.size_bytes
    )
}

fn absent_line(digest: &Digest) -> String {
    format!(r#"{{"digest":"{}","absent":true}}"#, label(digest))
}

/// The first line of an answer: the store and two grants, `grant-1` and `grant-2`.
fn header_line(store: &str) -> String {
    format!(r#"{{"store":"{store}","bucket":"b","grants":["{GRANT}","grant-2"]}}"#)
}

fn table(lines: &[String]) -> Table {
    let mut all = vec![header_line("http://store.test")];
    all.extend_from_slice(lines);
    Table::parse(all.join("\n").as_bytes())
}

fn located(
    mem: &Arc<Memory>,
    store: &Arc<FakeStore>,
    lines: &[String],
    spool: &Path,
) -> LocatedSource {
    LocatedSource::new(
        Box::new(Shared(Arc::clone(mem))),
        Some(Box::new(SharedStore(Arc::clone(store)))),
        table(lines),
        spool.to_owned(),
    )
}

/// A blob too big to be held in memory while it is checked.
const BIG: usize = HELD_IN_MEMORY as usize + 1000;

/// A writer that takes nothing.
struct Refuses;

impl Write for Refuses {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bodies(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| format!("input file number {i}, its own bytes").into_bytes())
        .collect()
}

/// A listener holding `blobs` and a root directory naming them `f0`, `f1`, ..., and the digests.
fn listener_with(blobs: &[Vec<u8>]) -> (Memory, Digest, Vec<Digest>) {
    let mut mem = Memory::default();
    let digests: Vec<Digest> = blobs.iter().map(|b| mem.put(b.clone())).collect();
    let files = digests
        .iter()
        .enumerate()
        .map(|(i, d)| file(&format!("f{i}"), d, false))
        .collect();
    let root = mem.put_directory(&Directory {
        files,
        ..Directory::default()
    });
    (mem, root, digests)
}

fn check_files(dir: &Path, blobs: &[Vec<u8>]) {
    for (i, blob) in blobs.iter().enumerate() {
        assert!(
            std::fs::read(dir.join(format!("f{i}"))).expect("file") == *blob,
            "f{i}"
        );
    }
}

// The table.

/// Catches a line read as the wrong kind: each pack and large line is a place with its key,
/// offset and length, and an absent line has none.
#[test]
fn a_table_places_pack_and_large_lines_and_not_absent_ones() {
    let (a, b, c) = (digest_of(b"aaa"), digest_of(&[7; 5000]), digest_of(b"ccc"));
    let table = table(&[
        pack_line(&a, "cas/f/w/p0", 40),
        large_line(&b),
        absent_line(&c),
    ]);
    assert_eq!(
        table.place(&a),
        Some(&Place::Pack {
            key: "cas/f/w/p0".to_owned(),
            offset: 40,
            length: 3
        })
    );
    assert_eq!(
        table.place(&b),
        Some(&Place::Large {
            key: format!("cas/l/{}", b.hash)
        })
    );
    assert_eq!(table.place(&c), None);
    assert_eq!(table.len(), 2);
    assert!(!table.is_empty());
    assert!(Table::parse(b"").is_empty());
}

/// Catches a line trusted that should not be: each of these is no place, and the good line after
/// it still is one, so a bad line neither poisons the table nor goes unnoticed. A grant index
/// that names no grant, or a grant that is not usable text, is a bad line too.
#[test]
fn a_line_the_task_cannot_trust_is_no_place() {
    let d = digest_of(b"payload");
    let good = pack_line(&d, "cas/f/w/p0", 8);
    let h = &d.hash;
    let span = |key: &str, extra: &str| {
        format!(
            r#"{{"digest":"{h}/7","pack":{{"key":"{key}","offset":0,"length":7{extra}}},"g":0}}"#
        )
    };
    let long_key = "a".repeat(1025);
    let bad = [
        "not json".to_owned(),
        "[]".to_owned(),
        r#"{"digest":"zz/7","pack":{"key":"cas/f/w/p","offset":0,"length":7},"g":0}"#.to_owned(),
        format!(
            r#"{{"digest":"{h}/-1","pack":{{"key":"cas/f/w/p","offset":0,"length":7}},"g":0}}"#
        ),
        format!(r#"{{"digest":"{h}/7","pack":{{"key":"cas/f/w/p","offset":0,"length":6}},"g":0}}"#),
        format!(
            r#"{{"digest":"{h}/7","pack":{{"key":"cas/f/w/p","offset":-1,"length":7}},"g":0}}"#
        ),
        format!(
            r#"{{"digest":"{h}/7","pack":{{"key":"cas/f/w/p","offset":18446744073709551615,"length":7}},"g":0}}"#
        ),
        span("", ""),
        span("/cas/f/w/p", ""),
        span("cas/../p", ""),
        span("cas/./p", ""),
        span("cas//p", ""),
        span("cas/p?x=1", ""),
        span("cas/p q", ""),
        span(&long_key, ""),
        format!(r#"{{"digest":"{h}/7","pack":{{"offset":0,"length":7}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","large":{{"key":"cas/l/other","size":7}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","large":{{"key":"cas/l/{h}","size":8}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","pack":5,"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","g":0}}"#),
        r#"{"digest":7,"pack":{"key":"p","offset":0,"length":7},"g":0}"#.to_owned(),
        // A digest with no size, a size that is not a number, and fields missing.
        format!(r#"{{"digest":"{h}","pack":{{"key":"cas/f/w/p","offset":0,"length":7}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/x","pack":{{"key":"cas/f/w/p","offset":0,"length":7}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","pack":{{"key":"cas/f/w/p","length":7}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","pack":{{"key":"cas/f/w/p","offset":0}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","large":{{"size":7}},"g":0}}"#),
        format!(r#"{{"digest":"{h}/7","large":{{"key":"cas/l/{h}"}},"g":0}}"#),
        // No grant index, a negative one, one past the grants, one that is not a number, and
        // ones naming a grant that is not usable: a space, empty, over 64 KiB, not text.
        span("cas/f/w/p", "").replace(r#","g":0"#, ""),
        span("cas/f/w/p", "").replace(r#""g":0"#, r#""g":-1"#),
        span("cas/f/w/p", "").replace(r#""g":0"#, r#""g":9"#),
        span("cas/f/w/p", "").replace(r#""g":0"#, r#""g":"0""#),
        span("cas/f/w/p", "").replace(r#""g":0"#, r#""g":2"#),
        span("cas/f/w/p", "").replace(r#""g":0"#, r#""g":3"#),
        span("cas/f/w/p", "").replace(r#""g":0"#, r#""g":4"#),
        span("cas/f/w/p", "").replace(r#""g":0"#, r#""g":5"#),
    ];
    let huge = "a".repeat(64 * 1024 + 1);
    let head = format!(
        r#"{{"store":"http://s","bucket":"b","grants":["{GRANT}","grant-2","has a space","{huge}","",7]}}"#
    );
    for line in bad {
        let table = Table::parse(format!("{head}\n{line}\n{good}\n").as_bytes());
        assert_eq!(table.len(), 1, "{line}");
        assert_eq!(
            table.place(&d).map(|p| matches!(p, Place::Pack { .. })),
            Some(true)
        );
        let alone = Table::parse(format!("{head}\n{line}\n").as_bytes());
        assert!(alone.is_empty(), "a line alone is no place: {line}");
    }
}

/// Catches the empty blob being read from a store (a range of no bytes is no request), a
/// repeated digest taking its later place (the first line wins), a key with `-`, `_` and `.`
/// being refused, and a second header replacing the first.
#[test]
fn the_empty_blob_has_no_place_and_the_first_line_of_a_digest_wins() {
    let empty = digest_of(b"");
    let d = digest_of(b"payload");
    let table = table(&[
        format!(
            r#"{{"digest":"{}","pack":{{"key":"cas/f/w/p","offset":4,"length":0}},"g":0}}"#,
            label(&empty)
        ),
        pack_line(&d, "cas/f/first-1_x.y", 1),
        pack_line(&d, "cas/f/second", 2),
        r#"{"store":"http://other","bucket":"other","grants":["other"]}"#.to_owned(),
    ]);
    assert_eq!(table.place(&empty), None);
    assert_eq!(
        table.place(&d),
        Some(&Place::Pack {
            key: "cas/f/first-1_x.y".to_owned(),
            offset: 1,
            length: 7
        })
    );
    assert_eq!(table.store(), Some(("http://store.test", "b")));
}

/// Catches the wrong grant or store being taken from the first line: each blob has the grant its
/// `g` names, the store and bucket are the line's, a line with no grants locates nothing (an
/// older server), a store that is not text is no store, and the token is not in the table's
/// `Debug` output.
#[test]
fn each_blob_has_the_grant_its_index_names_and_no_grants_means_no_places() {
    let (a, b) = (digest_of(b"first blob"), digest_of(b"second blob"));
    let table = table(&[
        pack_line_g(&a, "cas/f/w/p0", 4, 0),
        pack_line_g(&b, "cas/f/w/p1", 4, 1),
    ]);
    assert_eq!(table.locate(&a).map(|(_, g)| g), Some(GRANT));
    assert_eq!(table.locate(&b).map(|(_, g)| g), Some("grant-2"));
    assert_eq!(table.store(), Some(("http://store.test", "b")));
    let shown = format!("{table:?}");
    assert!(
        !shown.contains(GRANT) && !shown.contains("grant-2"),
        "{shown}"
    );
    let line = pack_line(&a, "cas/f/w/p0", 4);
    for head in [
        r#"{"store":"http://s","bucket":"b","grants":[]}"#,
        r#"{"store":"http://s","bucket":"b"}"#,
        r#"{"store":"http://s","bucket":"b","grants":"x"}"#,
        r#"{"grants":["g"]}"#,
        r#"{"store":7,"bucket":"b","grants":["g"]}"#,
        r#"{"store":"http://s","bucket":7,"grants":["g"]}"#,
    ] {
        let table = Table::parse(format!("{head}\n{line}\n").as_bytes());
        assert!(table.is_empty(), "{head}");
    }
    assert!(Table::parse(line.as_bytes()).is_empty());
    assert_eq!(Table::parse(b"").store(), None);
}
// The source.

/// Catches spans read from the listener, or read with the wrong range or no grant: three blobs in
/// one pack are laid out from three ranges of the store, presented with the grant, and the
/// listener is asked only for the root directory, which the table does not locate.
#[test]
fn spans_are_read_from_the_store_and_not_the_listener() {
    let blobs = bodies(3);
    let (pack, offsets) = pack_of(&blobs);
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack);
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let lines: Vec<String> = digests
        .iter()
        .zip(&offsets)
        .map(|(d, o)| pack_line(d, "cas/f/w/p0", *o))
        .collect();
    let dir = scratch("located-spans");
    let source = located(&mem, &store, &lines, &dir);
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    let wanted: Vec<(String, Want, String)> = digests
        .iter()
        .zip(&offsets)
        .map(|(d, o)| {
            (
                "cas/f/w/p0".to_owned(),
                Want::Range {
                    offset: *o,
                    length: d.size_bytes as u64,
                },
                GRANT.to_owned(),
            )
        })
        .collect();
    assert_eq!(store.reads(), wanted);
    assert!(digests.iter().all(|d| mem.reads(d) == 0));
    assert_eq!(mem.reads(&root), 1);
    assert_eq!(
        source.counts(),
        Counts {
            range: 3,
            large: 0,
            fallback: 0,
            absent: 1
        }
    );
}

/// Catches one bad span failing the run or spoiling its neighbours: the store has flipped bytes
/// for the middle blob, which alone is read from the listener, and all three files are right.
#[test]
fn a_corrupt_span_falls_back_for_that_blob_only() {
    let blobs = bodies(3);
    let mut stored = blobs.clone();
    stored[1][0] ^= 1;
    let (pack, offsets) = pack_of(&stored);
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack);
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let lines: Vec<String> = digests
        .iter()
        .zip(&offsets)
        .map(|(d, o)| pack_line(d, "cas/f/w/p0", *o))
        .collect();
    let dir = scratch("located-corrupt");
    let source = located(&mem, &store, &lines, &dir);
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    assert_eq!(
        [
            mem.reads(&digests[0]),
            mem.reads(&digests[1]),
            mem.reads(&digests[2])
        ],
        [0, 1, 0]
    );
    let counts = source.counts();
    assert_eq!((counts.range, counts.fallback), (2, 1));
    let report = source.report().expect("report");
    assert!(report.contains("2 by range"), "{report}");
    assert!(
        report.contains("1 by the listener after a failed read"),
        "{report}"
    );
}

/// Catches a short, a too long and a refused read being taken for the blob or failing the run:
/// each falls back to the listener, whose bytes are used.
#[test]
fn a_short_a_long_and_a_failed_read_fall_back() {
    let blobs = bodies(4);
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let store = Arc::new(FakeStore::default());
    let (full, offsets) = pack_of(&blobs);
    // The pack ends inside the first blob: a short read.
    store.put("cas/f/short", full[..offsets[0] as usize + 5].to_vec());
    store.put("cas/f/long", full.clone());
    store
        .overrun
        .lock()
        .expect("overrun")
        .insert("cas/f/long".to_owned());
    store.put("cas/f/refused", full);
    store
        .refuse
        .lock()
        .expect("refuse")
        .insert("cas/f/refused".to_owned());
    // No object "cas/f/gone": a 404.
    let lines = vec![
        pack_line(&digests[0], "cas/f/short", offsets[0]),
        pack_line(&digests[1], "cas/f/long", offsets[1]),
        pack_line(&digests[2], "cas/f/refused", offsets[2]),
        pack_line(&digests[3], "cas/f/gone", offsets[3]),
    ];
    let dir = scratch("located-failures");
    let source = located(&mem, &store, &lines, &dir);
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    assert!(digests.iter().all(|d| mem.reads(d) == 1));
    let counts = source.counts();
    assert_eq!((counts.range, counts.fallback), (0, 4));
}

/// Catches a large object not read whole from `cas/l/<hex>`, or left behind: the file is the
/// object, the store was asked for the whole of it, the listener was not asked, and the
/// spool leaves nothing in its directory.
#[test]
fn a_large_object_is_read_whole_and_checked() {
    let big: Vec<u8> = (0..BIG as u32).map(|i| (i % 251) as u8).collect();
    let (mem, root, digests) = listener_with(std::slice::from_ref(&big));
    let mem = Arc::new(mem);
    let store = Arc::new(FakeStore::default());
    store.put(&format!("cas/l/{}", digests[0].hash), big.clone());
    let dir = scratch("located-large");
    let source = located(&mem, &store, &[large_line(&digests[0])], &dir);
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &[big]);
    assert_eq!(
        store.reads(),
        vec![(
            format!("cas/l/{}", digests[0].hash),
            Want::Whole { size: BIG as u64 },
            GRANT.to_owned()
        )]
    );
    assert_eq!(mem.reads(&digests[0]), 0);
    assert_eq!(source.counts().large, 1);
    let names: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("in")]);
}

/// Catches a corrupt or refused large object reaching the file, or failing the run: the bytes of
/// the file are the listener's, and the corrupt bytes streamed from the store are not in it.
#[test]
fn a_large_object_that_is_corrupt_or_refused_falls_back_clean() {
    let big: Vec<u8> = (0..BIG as u32).map(|i| (i % 251) as u8).collect();
    let (mem, root, digests) = listener_with(std::slice::from_ref(&big));
    let mem = Arc::new(mem);
    let key = format!("cas/l/{}", digests[0].hash);
    for refuse in [false, true] {
        let store = Arc::new(FakeStore::default());
        let mut bad = big.clone();
        *bad.last_mut().expect("last") ^= 1;
        store.put(&key, bad);
        if refuse {
            store.refuse.lock().expect("refuse").insert(key.clone());
        }
        let dir = scratch(if refuse {
            "large-refused"
        } else {
            "large-corrupt"
        });
        let source = located(&mem, &store, &[large_line(&digests[0])], &dir);
        materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
        check_files(&dir.join("in"), std::slice::from_ref(&big));
        assert_eq!(source.counts().fallback, 1);
        assert_eq!(source.counts().large, 0);
    }
}

/// Catches a spool that cannot be made failing the run: with no directory for it the large
/// object is read from the listener.
#[test]
fn a_large_object_with_no_spool_directory_is_read_from_the_listener() {
    let big = vec![9u8; BIG];
    let (mem, root, digests) = listener_with(std::slice::from_ref(&big));
    let mem = Arc::new(mem);
    let store = Arc::new(FakeStore::default());
    store.put(&format!("cas/l/{}", digests[0].hash), big.clone());
    let dir = scratch("large-no-spool");
    let source = located(
        &mem,
        &store,
        &[large_line(&digests[0])],
        &dir.join("missing"),
    );
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &[big]);
    assert_eq!(source.counts().fallback, 1);
}

/// Catches blobs the table does not name being asked of the store, or an empty table doing
/// anything: the listener serves them all and the store sees nothing.
#[test]
fn a_blob_the_table_does_not_name_is_read_from_the_listener() {
    let blobs = bodies(2);
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let store = Arc::new(FakeStore::default());
    let lines = vec![absent_line(&digests[0])];
    let dir = scratch("located-absent");
    let source = located(&mem, &store, &lines, &dir);
    materialize(&source, &root, &dir.join("in"), 2).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    assert!(store.reads().is_empty());
    assert_eq!(
        source.counts(),
        Counts {
            range: 0,
            large: 0,
            fallback: 0,
            absent: 3
        }
    );
}

/// Catches the store being read when the answer carried no grants (an older server): the
/// listener serves every blob and the store sees nothing.
#[test]
fn a_table_without_grants_is_not_read_from_the_store() {
    let blobs = bodies(1);
    let (pack, offsets) = pack_of(&blobs);
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack);
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let line = format!(
        r#"{{"digest":"{}","pack":{{"key":"cas/f/w/p0","offset":{},"length":{}}}}}"#,
        label(&digests[0]),
        offsets[0],
        digests[0].size_bytes
    );
    let dir = scratch("located-no-grant");
    let source = LocatedSource::new(
        Box::new(Shared(Arc::clone(&mem))),
        Some(Box::new(SharedStore(Arc::clone(&store)))),
        Table::parse(line.as_bytes()),
        dir.to_path_buf(),
    );
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    assert!(store.reads().is_empty());
    assert_eq!(mem.reads(&digests[0]), 1);
    assert_eq!(source.counts().absent, 2);
}

/// Catches a table used when there is no store to read from: a source made with none serves
/// everything from the listener.
#[test]
fn a_source_with_no_store_reads_everything_from_the_listener() {
    let blobs = bodies(1);
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let (_, offsets) = pack_of(&blobs);
    let dir = scratch("located-no-store");
    let source = LocatedSource::new(
        Box::new(Shared(Arc::clone(&mem))),
        None,
        table(&[pack_line(&digests[0], "cas/f/w/p0", offsets[0])]),
        dir.to_path_buf(),
    );
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    assert_eq!(source.counts().absent, 2);
}

/// Catches a grant that fails for one key failing the others: the blobs of grant 1 are refused
/// by the store (an expired or too-narrow grant is a 403), and read from the listener, while
/// the blobs of grant 0 are read from the store.
#[test]
fn a_refused_grant_falls_back_for_its_blobs_only() {
    let blobs = bodies(3);
    let (pack, offsets) = pack_of(&blobs);
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack.clone());
    store.put("cas/f/w/p1", pack);
    store
        .refuse
        .lock()
        .expect("refuse")
        .insert("cas/f/w/p1".to_owned());
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let lines = vec![
        pack_line_g(&digests[0], "cas/f/w/p0", offsets[0], 0),
        pack_line_g(&digests[1], "cas/f/w/p1", offsets[1], 1),
        pack_line_g(&digests[2], "cas/f/w/p0", offsets[2], 0),
    ];
    let dir = scratch("located-refused-grant");
    let source = located(&mem, &store, &lines, &dir);
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    let presented: Vec<String> = store.reads().into_iter().map(|(_, _, g)| g).collect();
    assert_eq!(presented, vec![GRANT, "grant-2", GRANT]);
    assert_eq!(
        [
            mem.reads(&digests[0]),
            mem.reads(&digests[1]),
            mem.reads(&digests[2])
        ],
        [0, 1, 0]
    );
    let counts = source.counts();
    assert_eq!((counts.range, counts.fallback), (2, 1));
}
/// Catches `fetch_many` handing the caller's callback the wrong index when located and unlocated
/// blobs are mixed: each callback gets its own digest's bytes.
#[test]
fn fetch_many_keeps_the_callers_indexes_when_blobs_are_mixed() {
    let blobs = bodies(5);
    let (pack, offsets) = pack_of(&blobs);
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack);
    let (mem, _, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    // Blobs 1 and 3 are located; 0, 2 and 4 are not.
    let lines = vec![
        pack_line(&digests[1], "cas/f/w/p0", offsets[1]),
        pack_line(&digests[3], "cas/f/w/p0", offsets[3]),
    ];
    let dir = scratch("located-mixed");
    let source = located(&mem, &store, &lines, &dir);
    let got: Mutex<Vec<Vec<u8>>> = Mutex::new(vec![Vec::new(); 5]);
    source
        .fetch_many(&digests, 3, &|index, fetch| {
            let mut bytes = Vec::new();
            fetch(&mut bytes)?;
            got.lock().expect("got")[index] = bytes;
            Ok(())
        })
        .expect("fetch_many");
    assert!(*got.lock().expect("got") == blobs);
    assert_eq!(source.counts().range, 2);
    assert_eq!(source.counts().absent, 3);
}

/// Catches an error of the first phase being lost: a located blob that is gone from both the
/// store and the listener fails the call with a source error.
#[test]
fn fetch_many_returns_the_error_of_a_blob_nobody_has() {
    let gone = digest_of(b"nobody has this");
    let mem = Arc::new(Memory::default());
    let store = Arc::new(FakeStore::default());
    let dir = scratch("located-gone");
    let source = located(&mem, &store, &[pack_line(&gone, "cas/f/w/p0", 4)], &dir);
    let error = source
        .fetch_many(std::slice::from_ref(&gone), 1, &|_, fetch| {
            fetch(&mut Vec::new())
        })
        .expect_err("gone");
    assert!(matches!(error, FetchError::Source { .. }), "{error:?}");
}

/// Catches the fallback skipping the digest check: bytes that are wrong in the store and wrong
/// at the listener are a mismatch, which the task reports as exit 10, and nothing wrong is left
/// at the path.
#[test]
fn a_digest_mismatch_on_the_fallback_is_still_exit_10() {
    let blobs = bodies(1);
    let (mut mem, root, digests) = listener_with(&blobs);
    let mut wrong = blobs[0].clone();
    wrong[0] ^= 1;
    mem.blobs.insert(digests[0].hash.clone(), wrong.clone());
    let mem = Arc::new(mem);
    let (pack, offsets) = pack_of(&[wrong]);
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack);
    let lines = vec![pack_line(&digests[0], "cas/f/w/p0", offsets[0])];
    let dir = scratch("located-both-wrong");
    let source = located(&mem, &store, &lines, &dir);
    let error = materialize(&source, &root, &dir.join("in"), 1).expect_err("mismatch");
    assert!(matches!(error, FetchError::Mismatch { .. }), "{error:?}");
    assert!(!dir.join("in").join("f0").exists());
    let failure = crate::task::Failure::from(error);
    assert_eq!(failure.exit_code(), crate::task::EXIT_FETCH);
    assert_eq!(failure.exit_code(), 10);
}

/// Catches a span of any size being held in memory: a hostile answer can put a huge size on a pack
/// line, so a span over the in-memory limit is checked in the spool. With no spool directory the
/// read fails and the listener serves it, which shows the spool was the path; with one, the file
/// is right and nothing is left in the directory.
#[test]
fn a_span_over_the_memory_limit_is_checked_in_the_spool() {
    let big: Vec<u8> = (0..BIG as u32).map(|i| (i % 249) as u8).collect();
    let (pack, offsets) = pack_of(std::slice::from_ref(&big));
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack);
    let (mem, root, digests) = listener_with(std::slice::from_ref(&big));
    let mem = Arc::new(mem);
    let lines = vec![pack_line(&digests[0], "cas/f/w/p0", offsets[0])];
    let dir = scratch("span-spooled");
    let source = located(&mem, &store, &lines, &dir);
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), std::slice::from_ref(&big));
    assert_eq!((source.counts().range, source.counts().fallback), (1, 0));
    assert_eq!(mem.reads(&digests[0]), 0);
    let names: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("in")]);

    let dir = scratch("span-no-spool");
    let source = located(&mem, &store, &lines, &dir.join("missing"));
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), std::slice::from_ref(&big));
    assert_eq!((source.counts().range, source.counts().fallback), (0, 1));
}

/// Catches a header taken from later in the answer: only the first line is the header, so a
/// header-shaped line after a blank first line, or after an entry, sets no store.
#[test]
fn only_the_first_line_is_the_header() {
    let header = header_line("http://store.test");
    let d = digest_of(b"payload");
    let entry = pack_line(&d, "cas/f/w/p0", 4);
    assert!(Table::parse(format!("\n{header}\n{entry}\n").as_bytes()).is_empty());
    assert_eq!(
        Table::parse(format!("\n{header}\n").as_bytes()).store(),
        None
    );
    let after = Table::parse(format!("{entry}\n{header}\n").as_bytes());
    assert_eq!(after.store(), None);
    assert!(after.is_empty());
}

/// Catches a store that is down costing every blob a connect: the first blob's read cannot
/// connect, and the other blobs go to the listener without trying the store (the store is
/// asked once; the port is closed, so no clock is involved).
#[test]
fn a_store_that_cannot_be_reached_is_tried_once() {
    struct Counting {
        store: HttpObjectStore,
        asked: std::sync::atomic::AtomicUsize,
    }
    impl ObjectStore for Counting {
        fn read(
            &self,
            key: &str,
            want: Want,
            grant: &str,
            out: &mut dyn Write,
        ) -> Result<(), StoreError> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.store.read(key, want, grant, out)
        }
    }
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", free.local_addr().expect("addr"));
    drop(free);
    let blobs = bodies(3);
    let (pack, offsets) = pack_of(&blobs);
    drop(pack);
    let (mem, root, digests) = listener_with(&blobs);
    let mem = Arc::new(mem);
    let counting = Arc::new(Counting {
        store: HttpObjectStore::new(&base, "b", &[vec![1, 2, 3]], true).expect("store"),
        asked: std::sync::atomic::AtomicUsize::new(0),
    });
    struct Shared2(Arc<Counting>);
    impl ObjectStore for Shared2 {
        fn read(
            &self,
            key: &str,
            want: Want,
            grant: &str,
            out: &mut dyn Write,
        ) -> Result<(), StoreError> {
            self.0.read(key, want, grant, out)
        }
    }
    let lines: Vec<String> = digests
        .iter()
        .zip(&offsets)
        .map(|(d, o)| pack_line(d, "cas/f/w/p0", *o))
        .collect();
    let dir = scratch("store-down");
    let source = LocatedSource::new(
        Box::new(Shared(Arc::clone(&mem))),
        Some(Box::new(Shared2(Arc::clone(&counting)))),
        table(&lines),
        dir.to_path_buf(),
    );
    materialize(&source, &root, &dir.join("in"), 1).expect("materialize");
    check_files(&dir.join("in"), &blobs);
    assert_eq!(counting.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(source.counts().fallback, 3);
    assert!(digests.iter().all(|d| mem.reads(d) == 1));
}

#[path = "locate_http_tests.rs"]
mod http;

/// Catches: the listener's real `/locate` answer (kbf-front's `GET /locate` with a grant key, for a
/// fixed scope, source, key and clock) being a table the task cannot use. The file is the one
/// kbf-front's test `the_answer_is_the_golden_file_kbf_task_parses` compares with that answer;
/// kbf-front cannot be a dependency of this crate, so both read it. The store, the bucket, the
/// one grant, a pack place and a large place come out, and the absent lines locate nothing.
#[test]
fn a_real_answer_of_the_listener_is_a_table() {
    let golden = include_str!("../../kbf-front/tests/golden/locate_with_grants.ndjson");
    let table = Table::parse(golden.as_bytes());
    assert_eq!(
        table.store(),
        Some(("http://store.farm.test:9000", "tiers"))
    );
    assert_eq!(table.len(), 2);
    let pack = digest_of(b"alpha");
    let (place, grant) = table.locate(&pack).expect("the pack line is a place");
    assert_eq!(
        place,
        &Place::Pack {
            key: "cas/f/w/1".to_owned(),
            offset: 0,
            length: 5
        }
    );
    assert!(grant.starts_with("dGllcnN0b3JlLWdyYW50LXYxC"), "{grant}");
    let large = digest_of(&[7; 2000]);
    let (place, same) = table.locate(&large).expect("the large line is a place");
    assert_eq!(
        place,
        &Place::Large {
            key: format!("cas/l/{}", large.hash)
        }
    );
    assert_eq!(same, grant);
    assert_eq!(table.place(&digest_of(b"delta")), None);
}
