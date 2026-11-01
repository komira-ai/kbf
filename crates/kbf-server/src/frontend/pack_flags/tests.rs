//! The pack flags: what they need, what they make, and what opening them checks.

use std::os::unix::fs::PermissionsExt as _;

use clap::error::ErrorKind;
use kbf_pack::FRONTEND_PACK_BYTES;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_state::ManualClock;

use super::*;
use crate::frontend::{self, FrontendFlags};
use crate::test_dir::scratch_dir;

/// What a frontend needs of the command line apart from the pack flags.
const BASE: [&str; 5] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--api-listen=127.0.0.1:0",
];

/// The flags a pack store needs: the pack index (a Postgres state store, which is only parsed
/// here), the endpoint, the bucket and the credentials file.
const STORE: [&str; 5] = [
    "--state-store=postgres:///kbf?host=/run/postgresql",
    "--pack-index",
    "--pack-store-endpoint=http://127.0.0.1:9",
    "--pack-store-bucket=kbf-packs",
    "--pack-store-credentials-file=/etc/kbf/pack.json",
];

/// The flags of a frontend given `more` after the base.
fn parsed(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let argv = BASE.iter().chain(more).copied();
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// The kind of error the flags are refused with.
fn refused(more: &[&str]) -> ErrorKind {
    parsed(more).expect_err("refused").kind()
}

/// Asserts that nothing is packed without `--pack-store-endpoint`, and that the flags that have
/// a default, which the config template writes as live lines, change nothing and need nothing
/// when it is left out.
#[test]
fn the_pack_flags_are_off_unless_the_endpoint_is_given() {
    assert_eq!(parsed(&[]).expect("parses").packs, None);
    let companions = [
        "--pack-bytes=1048576",
        "--pack-writer=fe1",
        "--pack-store-region=eu-west-1",
        "--pack-inflight-bytes=67108864",
        "--pack-upload-idle-timeout=5",
        "--pack-commit-sample=5",
    ];
    assert_eq!(parsed(&companions).expect("parses").packs, None);
}

/// Asserts what each flag needs, so that a pack store is never half configured: the endpoint
/// needs the bucket, the credentials file and `--pack-index`; the bucket and the credentials
/// file need the endpoint; the cache directory needs the budget and the endpoint, and the
/// budget needs the directory.
#[test]
fn the_flags_need_each_other() {
    let missing = ErrorKind::MissingRequiredArgument;
    let state = STORE[0];
    let cases: [(&[&str], &str); 7] = [
        (
            &[state, "--pack-store-endpoint=http://h:9"],
            "no bucket, no file",
        ),
        (&[STORE[2], STORE[3], STORE[4]], "no pack index"),
        (
            &[state, STORE[1], STORE[2], STORE[3]],
            "no credentials file",
        ),
        (
            &[state, STORE[1], STORE[3], STORE[4]],
            "a bucket without an endpoint",
        ),
        (&[state, STORE[1], STORE[4]], "a file without an endpoint"),
        (
            &[state, "--pack-cache-bytes=1024"],
            "a budget without a directory",
        ),
        (
            &[state, "--pack-cache-dir=/var/cache/p"],
            "a directory without a budget",
        ),
    ];
    for (given, why) in cases {
        assert_eq!(refused(given), missing, "{why}");
    }
    let cache_only = [
        state,
        STORE[1],
        "--pack-cache-dir=/c",
        "--pack-cache-bytes=9",
    ];
    assert_eq!(refused(&cache_only), missing, "a cache without an endpoint");
    let zero = refused(&[state, "--pack-bytes=0"]);
    assert_eq!(zero, ErrorKind::ValueValidation);
}

/// Asserts that the flags make the setting they say, and that the defaults are the frontend's:
/// packs closed at 32 MiB, room for 1 GiB of uploads in flight, an idle timeout of 30 seconds,
/// the writer called `frontend`, the region `us-east-1`, no cache, two blobs of a committed pack
/// checked.
#[test]
fn the_flags_make_the_setting() {
    let plain = parsed(&STORE).expect("parses").packs.expect("packs");
    assert_eq!(
        plain,
        PackFlags {
            endpoint: "http://127.0.0.1:9".to_owned(),
            bucket: "kbf-packs".to_owned(),
            region: "us-east-1".to_owned(),
            credentials_file: PathBuf::from("/etc/kbf/pack.json"),
            pack_bytes: FRONTEND_PACK_BYTES,
            inflight_bytes: kbf_front::DEFAULT_INFLIGHT_BYTES,
            upload_idle_timeout: kbf_front::DEFAULT_UPLOAD_IDLE_TIMEOUT,
            writer: "frontend".to_owned(),
            cache: None,
            commit_sample: 2,
        }
    );
    let mut more = STORE.to_vec();
    more.extend([
        "--pack-store-region=eu-west-1",
        "--pack-bytes=4096",
        "--pack-inflight-bytes=134217728",
        "--pack-upload-idle-timeout=7",
        "--pack-writer=fe7",
        "--pack-cache-dir=/var/cache/packs",
        "--pack-cache-bytes=1000000",
        "--pack-commit-sample=9",
    ]);
    let set = parsed(&more).expect("parses").packs.expect("packs");
    assert_eq!(set.region, "eu-west-1");
    assert_eq!((set.pack_bytes, set.writer.as_str()), (4096, "fe7"));
    assert_eq!(set.inflight_bytes, 128 << 20);
    assert_eq!(set.upload_idle_timeout, Duration::from_secs(7));
    assert_eq!(set.commit_sample, 9);
    assert_eq!(
        set.cache,
        Some((PathBuf::from("/var/cache/packs"), 1_000_000))
    );
}

/// Asserts the bounds of `--pack-commit-sample`: 2 by default; 0, which checks no blob, only when
/// given as a value; 16384, the most blobs a pack holds (so that every blob is checked), at
/// most; and a number that is negative, past that or not a number is refused.
#[test]
fn the_commit_sample_is_two_unless_given_and_at_most_a_packs_blobs() {
    let most = i64::try_from(kbf_front::MAX_COMMIT_BLOBS);
    assert_eq!(most.expect("small"), MAX_COMMIT_SAMPLE);
    let sample = |given: &'static str| {
        let mut more = STORE.to_vec();
        more.push(given);
        parsed(&more).map(|flags| flags.packs.expect("packs").commit_sample)
    };

    let default = parsed(&STORE).expect("parses").packs.expect("packs");
    assert_eq!(default.commit_sample, 2);
    let none = sample("--pack-commit-sample=0");
    assert_eq!(none.expect("parses"), 0);
    let every = sample("--pack-commit-sample=16384");
    assert_eq!(every.expect("parses"), 16384);
    for bad in [
        "--pack-commit-sample=16385",
        "--pack-commit-sample=-1",
        "--pack-commit-sample=all",
    ] {
        let refused = sample(bad).expect_err("refused");
        assert_eq!(refused.kind(), ErrorKind::ValueValidation, "{bad}");
    }
}

/// Asserts the bounds of the room and idle-timeout flags: the room is refused below the largest
/// blob the packs take (64 MiB, so that no upload they accept is refused for its size) and one
/// past 4294967295 bytes, and accepted at both; the idle timeout is refused at 0 and one past a
/// day (86400 seconds), and accepted at the day.
#[test]
fn the_room_and_idle_flags_are_bounded() {
    let least = i64::try_from(kbf_front::MAX_PACKED_BLOB_BYTES);
    assert_eq!(least.expect("small"), MIN_INFLIGHT_BYTES);
    let state = STORE[0];
    for bad in [
        "--pack-inflight-bytes=0",
        "--pack-inflight-bytes=67108863",
        "--pack-inflight-bytes=4294967296",
        "--pack-upload-idle-timeout=0",
        "--pack-upload-idle-timeout=86401",
    ] {
        let kind = refused(&[state, bad]);
        assert_eq!(kind, ErrorKind::ValueValidation, "{bad}");
    }
    let mut smallest = STORE.to_vec();
    smallest.push("--pack-inflight-bytes=67108864");
    let set = parsed(&smallest).expect("parses").packs.expect("packs");
    assert_eq!(set.inflight_bytes, 64 << 20);
    let mut most = STORE.to_vec();
    most.extend([
        "--pack-inflight-bytes=4294967295",
        "--pack-upload-idle-timeout=86400",
    ]);
    let set = parsed(&most).expect("parses").packs.expect("packs");
    assert_eq!(set.inflight_bytes, u32::MAX);
    assert_eq!(set.upload_idle_timeout, Duration::from_secs(86_400));
}

/// The key pair file's text.
const KEYS: &str = r#"{"access_key_id": "AKIDEXAMPLE", "secret_access_key": "hunter2"}"#;

/// The file called `name` in `dir` of this text and mode.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

/// The text a refused credentials file is refused with.
fn why(path: &Path) -> String {
    read_credentials(path).expect_err("refused").to_string()
}

/// Asserts what is read from a credentials file: the pair of a file only its owner can read;
/// and that a file others can read, a file that is not there and a file that is not the pair
/// are refused with the file named and no part of the secret in the text.
#[test]
fn the_credentials_come_from_a_private_file() {
    let dir = scratch_dir("pack-credentials");
    for private in [0o600, 0o400] {
        let path = file(dir.path(), "private.json", KEYS, private);
        let credentials = read_credentials(&path).expect("read");
        let printed = format!("{credentials:?}");
        assert!(printed.contains("AKIDEXAMPLE"), "{printed}");
        assert!(!printed.contains("hunter2"), "{printed}");
    }

    let wide = file(dir.path(), "wide.json", KEYS, 0o640);
    let said = why(&wide);
    assert!(said.contains("mode 0640"), "{said}");
    assert!(said.contains("wide.json"), "{said}");
    assert!(!said.contains("hunter2"), "{said}");

    let missing = dir.path().join("none.json");
    assert!(why(&missing).contains("none.json"));

    let junk = file(dir.path(), "junk.json", "hunter2 is not JSON", 0o600);
    let said = why(&junk);
    assert!(said.contains("not JSON"), "{said}");
    assert!(!said.contains("hunter2 is"), "{said}");
}

/// The flags of a pack store in `dir`.
fn flags(dir: &Path) -> PackFlags {
    PackFlags {
        endpoint: "http://127.0.0.1:9".to_owned(),
        bucket: "kbf-packs".to_owned(),
        region: "us-east-1".to_owned(),
        credentials_file: file(dir, "pack-credentials.json", KEYS, 0o600),
        pack_bytes: 1 << 20,
        inflight_bytes: 1 << 20,
        upload_idle_timeout: Duration::from_secs(5),
        writer: "fe1".to_owned(),
        cache: None,
        commit_sample: 2,
    }
}

fn index() -> Arc<dyn PackIndex> {
    Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())))
}

/// Asserts that the flags open into packs (nothing is sent to the store to do it) that carry
/// the room for uploads and the idle timeout the flags say, with a cache when there is one, and
/// that each thing the flags name that cannot be used is refused naming its flag: an endpoint
/// the store client does not take, a writer name a key cannot hold, a cache directory that
/// cannot be made, a credentials file that is not there.
#[test]
fn the_flags_open_into_packs_or_are_refused_by_flag() {
    let dir = scratch_dir("pack-open");
    let mut ok = flags(dir.path());
    ok.cache = Some((dir.path().join("cache"), 4096));
    let packs = ok.open(index()).expect("opens");
    assert_eq!(packs.max_blob_bytes(), kbf_front::MAX_PACKED_BLOB_BYTES);
    assert_eq!(packs.inflight_bytes(), 1 << 20);
    assert_eq!(packs.upload_idle_timeout(), Duration::from_secs(5));
    assert!(dir.path().join("cache").is_dir());

    let mut tls = flags(dir.path());
    tls.endpoint = "https://127.0.0.1:9".to_owned();
    let said = tls.open(index()).expect_err("refused").to_string();
    assert!(said.starts_with("--pack-store-endpoint: "), "{said}");

    let mut slash = flags(dir.path());
    slash.writer = "a/b".to_owned();
    let said = slash.open(index()).expect_err("refused").to_string();
    assert!(said.contains("--pack-writer"), "{said}");

    let mut cache = flags(dir.path());
    cache.cache = Some((dir.path().join("pack-credentials.json").join("c"), 4096));
    let said = cache.open(index()).expect_err("refused").to_string();
    assert!(said.starts_with("--pack-cache-dir: "), "{said}");

    let mut gone = flags(dir.path());
    gone.credentials_file = dir.path().join("none.json");
    let said = gone.open(index()).expect_err("refused").to_string();
    assert!(said.starts_with("--pack-store-credentials-file "), "{said}");
}

/// Asserts the three cases of what a start does with the flags and the index: no flags, no
/// packs; flags and no index, a refusal that names the flag; flags and the index, packs.
#[test]
fn packs_need_both_the_flags_and_the_index() {
    let dir = scratch_dir("pack-start");
    let flags = flags(dir.path());
    let index = index();

    assert!(open(None, Some(&index)).expect("no flags").is_none());
    let said = open(Some(&flags), None).expect_err("no index").to_string();
    assert!(said.contains("--pack-index"), "{said}");
    assert!(open(Some(&flags), Some(&index)).expect("both").is_some());
}

/// Asserts when a frontend commits a daemon's packs: with packs and the flags that made them
/// both, and with neither alone; and that the commits check as many blobs as
/// `--pack-commit-sample` says.
#[test]
fn a_frontend_commits_packs_only_with_the_packs_and_their_flags() {
    let dir = scratch_dir("pack-commits");
    let flags = PackFlags {
        commit_sample: 7,
        ..flags(dir.path())
    };
    let packs = flags.open(index()).expect("opens");

    let both = commits(Some(&packs), Some(&flags)).expect("both");
    assert_eq!(both.sample(), 7);
    assert!(commits(Some(&packs), None).is_none());
    assert!(commits(None, Some(&flags)).is_none());
    assert!(commits(None, None).is_none());
}

/// Asserts that a boot id is the start time in microseconds in hex, a dash, and the process id
/// in hex, the parts the keys of a start are named apart by.
#[test]
fn a_boot_id_is_made_of_the_time_and_the_process() {
    let id = boot_id();
    let (micros, pid) = id.split_once('-').expect("a dash");
    assert_eq!(pid, format!("{:x}", std::process::id()));
    assert!(u128::from_str_radix(micros, 16).expect("hex") > 0);
}
