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
    fn read(&self, key: &str, want: Want, grant: &str, out: &mut dyn Write) -> Result<(), String> {
        let store = &self.0;
        store
            .reads
            .lock()
            .expect("reads")
            .push((key.to_owned(), want, grant.to_owned()));
        if store.refuse.lock().expect("refuse").contains(key) {
            return Err("403".to_owned());
        }
        let objects = store.objects.lock().expect("objects");
        let object = objects.get(key).ok_or_else(|| "404".to_owned())?;
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
            .map_err(|e| e.to_string())?;
        if store.overrun.lock().expect("overrun").contains(key) {
            out.write_all(b"x").map_err(|e| e.to_string())?;
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
    let big: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
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
            Want::Whole { size: 50_000 },
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
    let big: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
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
    let big = vec![9u8; 4096];
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

// `/locate` over HTTP.

fn plain_listener(
    handler: impl Fn(&crate::fake_http::Seen) -> Answer + Send + Sync + 'static,
) -> (Fake, HttpBlobSource) {
    let fake = Fake::start(handler);
    let source =
        HttpBlobSource::new_with(&fake.base, "tok", &[vec![1, 2, 3]], true).expect("source");
    (fake, source)
}

/// Catches the request being wrong: `GET /locate` with the bearer token, and the body returned.
#[test]
fn locate_asks_the_listener_with_the_bearer() {
    let (fake, source) = plain_listener(|_| Answer::new(200, "the table"));
    assert_eq!(source.locate().expect("locate"), b"the table");
    let seen = fake.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("GET", "/locate")
    );
    assert_eq!(seen[0].header("authorization"), Some("Bearer tok"));
}

/// Catches an answer other than 200 being used, and an over-long body being read whole: 503,
/// 404 and 401 are errors naming the status and not the body, and a body past the cap is refused.
#[test]
fn locate_is_an_error_unless_the_listener_answers_200_within_the_cap() {
    for status in [503u16, 404, 401] {
        let (_fake, source) = plain_listener(move |_| Answer::new(status, "store_unavailable"));
        let error = source.locate().expect_err("refused").to_string();
        assert!(error.contains(&status.to_string()), "{error}");
        assert!(!error.contains("store_unavailable"), "{error}");
    }
    let (_fake, source) = plain_listener(|_| Answer::new(200, vec![b'x'; 11]));
    assert_eq!(source.locate_within(11).expect("at the cap").len(), 11);
    let error = source.locate_within(10).expect_err("over").to_string();
    assert!(error.contains("10"), "{error}");
}

/// Catches a listener that cannot be reached being an error that stops the run: it is an error
/// value, which `open` turns into an empty table.
#[test]
fn locate_of_a_listener_that_is_not_there_is_an_error() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", free.local_addr().expect("addr"));
    drop(free);
    let source = HttpBlobSource::new_with(&base, "tok", &[vec![1, 2, 3]], true).expect("source");
    assert!(source.locate().is_err());
}

/// A listener that serves `blobs` by `/<hex>/<size>` and `locate` at `/locate`.
fn listener_serving(blobs: &[Vec<u8>], locate: Answer) -> (Fake, HttpBlobSource) {
    let by_path: HashMap<String, Vec<u8>> = blobs
        .iter()
        .map(|b| (format!("/{}", label(&digest_of(b))), b.clone()))
        .collect();
    let locate = Arc::new(Mutex::new(Some(locate)));
    plain_listener(move |seen| {
        if seen.path == "/locate" {
            let answer = locate.lock().expect("locate");
            let answer = answer.as_ref().expect("answer");
            return Answer::new(answer.status, answer.body.clone());
        }
        match by_path.get(&seen.path) {
            Some(body) => Answer::new(200, body.clone()),
            None => Answer::new(404, ""),
        }
    })
}

/// A store server holding `objects`, answering ranges as the store does: 206 for a `Range`, 200
/// for none, 403 for a missing grant header.
fn store_serving(objects: HashMap<String, Vec<u8>>) -> Fake {
    Fake::start(move |seen| {
        if seen.header("x-tierstore-grant").is_none() {
            return Answer::new(403, "");
        }
        let Some(object) = seen
            .path
            .strip_prefix("/b/")
            .and_then(|key| objects.get(key))
        else {
            return Answer::new(404, "");
        };
        match seen.header("range") {
            None => Answer::new(200, object.clone()),
            Some(range) => {
                let (from, to) = range
                    .strip_prefix("bytes=")
                    .and_then(|r| r.split_once('-'))
                    .expect("range");
                let (from, to): (usize, usize) =
                    (from.parse().expect("from"), to.parse().expect("to"));
                Answer::new(206, object[from..=to].to_vec())
            }
        }
    })
}

fn open(listener: HttpBlobSource, dir: &Path) -> LocatedSource {
    LocatedSource::open(listener, &[vec![1, 2, 3]], true, dir.to_path_buf())
}

/// Catches `open` asking more than once, or not at all, or reading the listener for a located
/// blob, or the store being the answer's: one `/locate`, the located blob from the store the
/// answer names (with the grant its line names), the other from the listener.
#[test]
fn open_asks_locate_once_and_reads_where_it_points() {
    let blobs = bodies(2);
    let (pack, offsets) = pack_of(&blobs[..1]);
    let store = store_serving(HashMap::from([("cas/f/w/p0".to_owned(), pack)]));
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    let body = [
        header_line(&store.base),
        pack_line_g(&digests[0], "cas/f/w/p0", offsets[0], 1),
        absent_line(&digests[1]),
    ]
    .join("\n");
    let (fake, listener) = listener_serving(&blobs, Answer::new(200, body));
    let dir = scratch("open-once");
    let source = open(listener, &dir);
    assert!(source.get(&digests[0]).expect("first") == blobs[0]);
    assert!(source.get(&digests[1]).expect("second") == blobs[1]);
    let paths = fake.paths();
    assert_eq!(paths.iter().filter(|p| *p == "/locate").count(), 1);
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[1], format!("/{}", label(&digests[1])));
    let seen = store.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("x-tierstore-grant"), Some("grant-2"));
    assert_eq!(seen[0].path, "/b/cas/f/w/p0");
    assert_eq!(source.counts().range, 1);
}

/// Catches an unusable `/locate` stopping the run: a 503, a 404, a body that is no table and a
/// table with no grants all leave every blob to the listener, and the report names why.
#[test]
fn a_locate_that_fails_leaves_every_blob_to_the_listener() {
    let blobs = bodies(2);
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    let (pack, offsets) = pack_of(&blobs[..1]);
    let store = store_serving(HashMap::from([("cas/f/w/p0".to_owned(), pack)]));
    let head = format!(r#"{{"store":"{}","bucket":"b","grants":[]}}"#, store.base);
    let no_grants = format!(
        "{head}\n{}",
        pack_line(&digests[0], "cas/f/w/p0", offsets[0])
    );
    for answer in [
        Answer::new(503, "store_unavailable\n"),
        Answer::new(404, ""),
        Answer::new(200, "this is not a table\n"),
        Answer::new(200, no_grants),
    ] {
        let status = answer.status;
        let (fake, listener) = listener_serving(&blobs, answer);
        let dir = scratch("locate-fails");
        let source = open(listener, &dir);
        for (d, blob) in digests.iter().zip(&blobs) {
            assert!(source.get(d).expect("get") == *blob);
        }
        assert_eq!(source.counts().absent, 2);
        assert_eq!(fake.paths().len(), 3);
        let report = source.report().expect("report");
        if status != 200 {
            assert!(report.contains(&status.to_string()), "{report}");
        }
    }
    assert!(store.requests().is_empty());
}

/// Catches a store the task should not use being used: an `http` store without the opt-in, a
/// store that is not a URL, and a bucket that is not a plain name all leave the listener to serve
/// every blob and the store unasked.
#[test]
fn a_store_the_task_will_not_use_is_not_used() {
    let blobs = bodies(1);
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    let (pack, offsets) = pack_of(&blobs);
    let store = store_serving(HashMap::from([("cas/f/w/p0".to_owned(), pack)]));
    let entry = pack_line(&digests[0], "cas/f/w/p0", offsets[0]);
    let cases = [
        (
            format!(
                r#"{{"store":"{}","bucket":"b","grants":["g"]}}"#,
                store.base
            ),
            false,
        ),
        (
            r#"{"store":"ftp://x","bucket":"b","grants":["g"]}"#.to_owned(),
            true,
        ),
        (
            format!(
                r#"{{"store":"{}","bucket":"Bad Name","grants":["g"]}}"#,
                store.base
            ),
            true,
        ),
    ];
    for (head, allow_plain) in cases {
        let (_fake, listener) =
            listener_serving(&blobs, Answer::new(200, format!("{head}\n{entry}")));
        let dir = scratch("store-unused");
        let source =
            LocatedSource::open(listener, &[vec![1, 2, 3]], allow_plain, dir.to_path_buf());
        assert!(source.get(&digests[0]).expect("get") == blobs[0]);
        assert_eq!(source.counts().absent, 1, "{head}");
    }
    assert!(store.requests().is_empty());
}

// The store over HTTP.

fn store_at(fake: &Fake, bucket: &str) -> HttpObjectStore {
    HttpObjectStore::new(&fake.base, bucket, &[vec![1, 2, 3]], true).expect("store")
}

/// Catches the wrong request to the store: path-style `/<bucket>/<key>`, a `Range` of the right
/// bytes, the grant in `x-tierstore-grant` and no `Authorization`; a 206 is the bytes asked for.
#[test]
fn the_store_is_asked_for_a_range_with_the_grant_header() {
    let object: Vec<u8> = (0..100u8).collect();
    let body = object.clone();
    let fake = Fake::start(move |_| Answer::new(206, body[10..20].to_vec()));
    let store = store_at(&fake, "kbf-bucket");
    let mut out = Vec::new();
    store
        .read(
            "cas/f/w/p0",
            Want::Range {
                offset: 10,
                length: 10,
            },
            "the-grant",
            &mut out,
        )
        .expect("range");
    assert_eq!(out, object[10..20]);
    let seen = &fake.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("GET", "/kbf-bucket/cas/f/w/p0")
    );
    assert_eq!(seen.header("range"), Some("bytes=10-19"));
    assert_eq!(seen.header("x-tierstore-grant"), Some("the-grant"));
    assert_eq!(seen.header("authorization"), None);
}

/// Catches a whole object read with a `Range`, or a 200 taken for a range: a whole read has no
/// `Range` header and wants 200; a range read wants 206.
#[test]
fn a_whole_read_wants_200_and_a_range_read_wants_206() {
    let fake = Fake::start(|_| Answer::new(200, "whole"));
    let store = store_at(&fake, "b");
    let mut out = Vec::new();
    store
        .read("cas/l/abc", Want::Whole { size: 5 }, "g", &mut out)
        .expect("whole");
    assert_eq!(out, b"whole");
    assert_eq!(fake.requests()[0].header("range"), None);
    let error = store
        .read(
            "cas/f/p",
            Want::Range {
                offset: 0,
                length: 5,
            },
            "g",
            &mut Vec::new(),
        )
        .expect_err("200 for a range");
    assert!(error.contains("200"), "{error}");
    let fake = Fake::start(|_| Answer::new(206, "part"));
    let error = store_at(&fake, "b")
        .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Vec::new())
        .expect_err("206 for whole");
    assert!(error.contains("206"), "{error}");
}

/// Catches a refusal being read as bytes: 403 and 404 are errors naming the status, and an
/// unreachable store is an error, and a grant that cannot be a header value is one.
#[test]
fn a_refusal_or_an_unreachable_store_is_an_error() {
    for status in [403u16, 404, 500] {
        let fake = Fake::start(move |_| Answer::new(status, "AccessDenied"));
        let error = store_at(&fake, "b")
            .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Vec::new())
            .expect_err("refused");
        assert!(error.contains(&status.to_string()), "{error}");
        assert!(!error.contains("AccessDenied"), "{error}");
    }
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", free.local_addr().expect("addr"));
    drop(free);
    let store = HttpObjectStore::new(&base, "b", &[vec![1, 2, 3]], true).expect("store");
    assert!(
        store
            .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Vec::new())
            .is_err()
    );
    let fake = Fake::start(|_| Answer::new(200, "x"));
    assert!(
        store_at(&fake, "b")
            .read(
                "cas/l/abc",
                Want::Whole { size: 1 },
                "bad\nvalue",
                &mut Vec::new()
            )
            .is_err()
    );
}

/// Catches a store that cannot write: when the writer refuses, the read is an error.
#[test]
fn a_writer_that_refuses_ends_the_store_read() {
    struct Refuses;
    impl Write for Refuses {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let fake = Fake::start(|_| Answer::new(200, "data"));
    let error = store_at(&fake, "b")
        .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Refuses)
        .expect_err("refused");
    assert!(error.contains("full"), "{error}");
}

/// Catches a store made from a bad name, a bad scheme or with no roots: no roots, a bucket that
/// is empty or has a slash, a capital or a space, and an endpoint that is not `https` (or `http`
/// with the opt-in) are errors, and a trailing slash on the endpoint is dropped.
#[test]
fn a_store_needs_roots_a_plain_bucket_name_and_a_scheme_it_may_use() {
    let roots = [vec![1, 2, 3]];
    assert!(HttpObjectStore::new("http://h:1", "b", &[], true).is_err());
    for bucket in ["", "a/b", "Upper", "a b", "a?b"] {
        assert!(
            HttpObjectStore::new("http://h:1", bucket, &roots, true).is_err(),
            "{bucket:?}"
        );
    }
    assert!(HttpObjectStore::new("http://h:1/", "my-bucket.1", &roots, true).is_ok());
    assert!(HttpObjectStore::new("HTTPS://h:1", "b", &roots, false).is_ok());
    assert!(HttpObjectStore::new("http://h:1", "b", &roots, false).is_err());
    assert!(HttpObjectStore::new("ftp://h:1", "b", &roots, true).is_err());
    let fake = Fake::start(|_| Answer::new(200, "x"));
    let store = HttpObjectStore::new(&format!("{}/", fake.base), "b", &roots, true).expect("store");
    store
        .read("cas/l/abc", Want::Whole { size: 1 }, "g", &mut Vec::new())
        .expect("read");
    assert_eq!(fake.paths(), vec!["/b/cas/l/abc".to_owned()]);
}
