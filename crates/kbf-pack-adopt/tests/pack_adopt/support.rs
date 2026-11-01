//! Fixtures: an S3 service on loopback holding segments, the credentials file the tool reads, an
//! in-memory pack index, and runs of the tool over them.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::s3::{S3Store, fake::FakeS3};
use kbf_objstore::{Capabilities, ObjectKey, ObjectStore};
use kbf_pack::{Fetch, Reader, ReaderConfig, blob_digest};
use kbf_pack_adopt::{Seams, main_with, main_with_seams};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, PackIndex, PackIndexError};
use kbf_segments::{MAX_SEGMENT_BYTES, SegmentWriter};
use kbf_state::ManualClock;
use kbf_state::postgres::PostgresConfig;
use kbf_testkit::dir::TestDir;

/// The URL given to the flag when a seam opens the index: parsed, and never connected to.
pub const SOCKET_URL: &str = "postgres:///kbf?host=/run/postgresql";

/// The bucket of the fake S3 service.
pub const BUCKET: &str = "kbf-fake";

/// What a run of the tool printed and ended with.
pub struct Ran {
    pub code: u8,
    pub out: String,
    pub err: String,
}

/// A fake S3 service with three segments in it, one under each key layout of the members, and
/// an empty in-memory pack index.
pub struct Farm {
    pub dir: TestDir,
    pub runtime: tokio::runtime::Runtime,
    pub s3: FakeS3,
    pub index: MemoryPackIndex,
    /// The keys of the segments, in the order they are listed.
    pub keys: Vec<String>,
    /// The five blobs in the segments.
    pub blobs: Vec<Bytes>,
    /// The credentials file the tool is given.
    pub credentials: PathBuf,
}

impl Farm {
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime");
        let s3 = runtime.block_on(FakeS3::start());
        let credentials = dir.path().join("keys.json");
        write_secret(&credentials, 0o600);
        let mut farm = Self {
            dir,
            runtime,
            s3,
            index: MemoryPackIndex::new(Arc::new(ManualClock::default())),
            keys: Vec::new(),
            blobs: Vec::new(),
            credentials,
        };
        farm.seed();
        farm
    }

    /// Puts the three segments in the bucket.
    fn seed(&mut self) {
        let texts = ["alpha", "bravo", "charlie", "delta", "echo"];
        self.blobs = texts
            .iter()
            .map(|text| Bytes::copy_from_slice(text.as_bytes()))
            .collect();
        // In the order of a listing: the log's key sorts before the others.
        self.keys = vec![
            "kbf/0000000000000006/cas/0000000000000007/0000000000000008".to_owned(),
            "kbf/cas/0000000000000001/0000000000000002/0000000000000003".to_owned(),
            "kbf/cas/0000000000000004/0000000000000005".to_owned(),
        ];
        let ranges = [3..5, 0..2, 2..3];
        for (key, range) in self.keys.iter().zip(ranges) {
            let mut writer = SegmentWriter::new(MAX_SEGMENT_BYTES);
            for blob in &self.blobs[range] {
                writer.push(blob).expect("the blob fits");
            }
            let key = ObjectKey::new(key.as_str()).unwrap();
            let body = Bytes::from(writer.finish());
            let store = self.store();
            let put = store.put_new(&key, body, None);
            self.runtime.block_on(put).unwrap();
        }
    }

    /// A client of the fake bucket.
    pub fn store(&self) -> S3Store {
        self.s3.store(Capabilities {
            conditional_put: true,
            object_lock: false,
        })
    }

    /// The digests of the five blobs.
    pub fn digests(&self) -> Vec<BlobDigest> {
        self.blobs.iter().map(|bytes| blob_digest(bytes)).collect()
    }

    /// What the reader finds for the five blobs, through the farm's index and the fake bucket.
    pub fn read_all(&self) -> Vec<Fetch> {
        self.read_through(Arc::new(self.index.handle()))
    }

    /// What the reader finds for the five blobs, through `index` and the fake bucket.
    pub fn read_through(&self, index: Arc<dyn PackIndex>) -> Vec<Fetch> {
        let store = Arc::new(self.store());
        let reader = Reader::new(store, index, None, ReaderConfig::default());
        let wanted = self.digests();
        self.runtime.block_on(reader.read(&wanted)).expect("read")
    }

    /// `Fetch::Found` for each of the five blobs.
    pub fn found_all(&self) -> Vec<Fetch> {
        let found = |bytes: &Bytes| Fetch::Found(bytes.clone());
        self.blobs.iter().map(found).collect()
    }

    /// Seams that open the farm's in-memory index.
    pub fn seams(&self) -> Seams {
        let index = self.index.handle();
        let open = move |_: &PostgresConfig| -> Result<Arc<dyn PackIndex>, PackIndexError> {
            Ok(Arc::new(index.handle()))
        };
        Seams {
            open_index: Box::new(open),
        }
    }

    /// The command line up to `--pack-index`: the source store.
    fn base(&self) -> Vec<OsString> {
        let mut argv = vec![OsString::from("kbf-pack-adopt")];
        let flags = [
            "--source-store-endpoint",
            self.s3.endpoint(),
            "--source-store-bucket",
            BUCKET,
            "--source-store-credentials-file",
        ];
        argv.extend(flags.iter().map(|flag| OsString::from(*flag)));
        argv.push(self.credentials.clone().into_os_string());
        argv
    }

    /// Runs the tool with the farm's index and `extra` flags after the source store's.
    pub fn run(&self, extra: &[&str]) -> Ran {
        self.run_with(&self.seams(), SOCKET_URL, extra)
    }

    /// The command line with this `--pack-index` and `extra` flags after the source store's.
    pub fn argv(&self, url: &str, extra: &[&str]) -> Vec<OsString> {
        let mut argv = self.base();
        argv.push(OsString::from("--pack-index"));
        argv.push(OsString::from(url));
        argv.extend(extra.iter().map(|flag| OsString::from(*flag)));
        argv
    }

    /// Runs the tool with these seams and this `--pack-index`.
    pub fn run_with(&self, seams: &Seams, url: &str, extra: &[&str]) -> Ran {
        let argv = self.argv(url, extra);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = main_with_seams(argv, seams, &mut out, &mut err);
        Ran {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    /// Runs the tool as `main` does, opening the index on Postgres.
    pub fn run_for_real(&self, url: &str) -> Ran {
        let mut argv = self.base();
        argv.push(OsString::from("--pack-index"));
        argv.push(OsString::from(url));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = main_with(argv, &mut out, &mut err);
        Ran {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    /// A path in the farm's directory.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// Asserts that `text` holds `wanted`.
pub fn says(text: &str, wanted: &str) {
    assert!(text.contains(wanted), "{wanted:?} is not in {text:?}");
}

/// A credentials file with this mode.
pub fn write_secret(path: &Path, mode: u32) {
    let keys = r#"{"access_key_id": "k", "secret_access_key": "s"}"#;
    write_file(path, keys, mode);
}

/// A file with this text and this mode.
pub fn write_file(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).expect("written");
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions).expect("mode set");
}
