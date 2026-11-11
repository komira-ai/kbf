use super::*;
use crate::channels::apt::build::{self, KEEP};
use crate::channels::apt::read::read;
use crate::channels::apt::testing::*;
use kbf_testkit::dir::TestDir;

const SIGNED: [(&str, &str); 2] = [("Release.gpg", "signature"), ("InRelease", "clear signed")];

struct Repo {
    dir: TestDir,
    debs: TestDir,
    aws: FakeAws,
}

fn key(rel: &str) -> String {
    format!("apt/dists/stable/{rel}")
}

impl Repo {
    /// A repository another product published into, read, merged with release `version` and
    /// signed (the signatures are text).
    fn new(name: &str, version: &str) -> Repo {
        let dir = scratch(name);
        let debs = scratch(&format!("{name}-debs"));
        release_files(&debs, version);
        let aws = FakeAws::new();
        aws.put(
            &key("Release"),
            b"Origin: komira\nSHA256:\n aa 1 main/binary-amd64/Packages\n",
        );
        aws.put(
            &key("main/binary-amd64/Packages"),
            b"Package: another\nVersion: 9.9.9\nArchitecture: amd64\nSHA256: ee\n\n",
        );
        aws.put(&key("InRelease"), b"old clear signed");
        read(&aws, Suite::Stable, BUCKET, &dir.join("current")).expect("read");
        build::build(&build::Request {
            suite: Suite::Stable,
            current: &dir.join("current"),
            out: &dir.join("next"),
            debs: &debs,
            commit: COMMIT,
            version,
            origin: "komira",
            label: "komira",
            description: "komira packages",
            date: 1_790_000_000,
            keep: KEEP,
        })
        .expect("build");
        for (name, text) in SIGNED {
            std::fs::write(dir.join("next/dists/stable").join(name), text).expect("sign");
        }
        Repo { dir, debs, aws }
    }

    fn request<'a>(&'a self, last: bool) -> Request<'a> {
        Request {
            suite: Suite::Stable,
            bucket: BUCKET,
            distribution_id: "E123ABC",
            hostname: "packages.komira.ai",
            next: Box::leak(self.dir.join("next").into_boxed_path()),
            pool: Some(Pool {
                debs: &self.debs,
                commit: COMMIT,
                version: "1.2.3",
            }),
            last,
        }
    }

    fn out(&self) -> std::path::PathBuf {
        self.dir.join("out")
    }

    fn written(&self) -> String {
        std::fs::read_to_string(self.out()).unwrap_or_default()
    }
}

fn cond(call: &[String]) -> (String, String) {
    for name in ["--if-match", "--if-none-match"] {
        if let Some(value) = flag(call, name) {
            return (name.to_owned(), value);
        }
    }
    panic!("a put without a condition: {call:?}");
}

#[test]
fn the_packages_go_first_then_the_by_hash_copies_then_the_index_with_in_release_last() {
    // Catches: an index that lists a package that is not in the pool yet, a client that cannot find
    // the Packages its Release lists, and a commit point that is not the last write.
    let repo = Repo::new("write-order", "1.2.3");
    let said = write(&repo.aws, &repo.request(false), Some(&repo.out())).expect("write");
    let keys = repo.aws.keys_put();
    let packages = std::fs::read(
        repo.dir
            .join("next/dists/stable/main/binary-amd64/Packages"),
    )
    .expect("packages");
    let by_hash = |rel: &str, bytes: &[u8]| {
        format!(
            "apt/dists/stable/{}/by-hash/SHA256/{}",
            rel.rsplit_once('/').expect("dir").0,
            sha256_hex(bytes)
        )
    };
    let read_next =
        |rel: &str| std::fs::read(repo.dir.join("next/dists/stable").join(rel)).expect("file");
    let expected = [
        "apt/pool/kbf/kbf_1.2.3_amd64.deb".to_owned(),
        "apt/pool/kbf/kbf_1.2.3_arm64.deb".to_owned(),
        by_hash("main/binary-amd64/Packages", &packages),
        by_hash(
            "main/binary-amd64/Packages.gz",
            &read_next("main/binary-amd64/Packages.gz"),
        ),
        by_hash(
            "main/binary-arm64/Packages",
            &read_next("main/binary-arm64/Packages"),
        ),
        by_hash(
            "main/binary-arm64/Packages.gz",
            &read_next("main/binary-arm64/Packages.gz"),
        ),
        key("main/binary-amd64/Packages"),
        key("main/binary-amd64/Packages.gz"),
        key("main/binary-arm64/Packages"),
        key("main/binary-arm64/Packages.gz"),
        key("Release"),
        key("Release.gpg"),
        key("InRelease"),
    ];
    assert_eq!(keys, expected);
    // Packages and by-hash copies are never overwritten; an index object that was read is replaced
    // only if it is still the version that was read, one that did not exist only if still absent.
    let puts = repo.aws.calls_of("put-object");
    for call in &puts[..6] {
        assert_eq!(cond(call), ("--if-none-match".to_owned(), "*".to_owned()));
    }
    let etags = std::fs::read_to_string(repo.dir.join("next/etags.txt")).expect("etags");
    let read_etag = |rel: &str| {
        etags
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{rel}\t")))
            .expect("etag")
            .to_owned()
    };
    assert_eq!(
        cond(&puts[6]),
        (
            "--if-match".to_owned(),
            read_etag("main/binary-amd64/Packages")
        )
    );
    assert_eq!(
        cond(&puts[7]),
        ("--if-none-match".to_owned(), "*".to_owned())
    );
    assert_eq!(
        cond(&puts[12]),
        ("--if-match".to_owned(), read_etag("InRelease"))
    );
    assert_eq!(read_etag("Release.gpg"), "-");
    // What is stored is what was built, and the other product's entry is still in it.
    assert_eq!(
        repo.aws.get(&key("InRelease")).expect("stored"),
        b"clear signed"
    );
    let stored = String::from_utf8(
        repo.aws
            .get(&key("main/binary-amd64/Packages"))
            .expect("stored"),
    )
    .expect("utf8");
    assert!(stored.contains("another") && stored.contains("kbf"));
    // The CDN is told last, and the result is written.
    let invalidation = &repo.aws.calls.borrow().last().expect("last").clone();
    assert_eq!(
        invalidation[1..],
        [
            "create-invalidation",
            "--distribution-id",
            "E123ABC",
            "--paths",
            "/apt/dists/*"
        ]
    );
    assert_eq!(repo.written(), "conflict=false\n");
    assert_eq!(
        said.last().expect("last"),
        "invalidated /apt/dists/*; the index is at https://packages.komira.ai/apt/dists/stable/InRelease"
    );
}

#[test]
fn a_lost_index_race_ends_green_asking_for_another_attempt_and_never_signs_over_the_winner() {
    // Catches: overwriting an index another product wrote in between, and a lost race that fails
    // the job instead of retrying.
    let repo = Repo::new("write-race", "1.2.3");
    repo.aws.before_put_of(
        "dists/stable/Release.gpg",
        Box::new(|store| {
            store.insert(key("Release.gpg"), b"another product's signature".to_vec());
        }),
    );
    let said =
        write(&repo.aws, &repo.request(false), Some(&repo.out())).expect("a lost race is green");
    assert_eq!(repo.written(), "conflict=true\n");
    assert_eq!(
        said.last().expect("last"),
        "::notice title=apt index::apt/dists/stable/Release.gpg changed while the index was built; the next attempt reads it again"
    );
    // InRelease, the commit point, was not written, and the CDN was not told.
    assert_eq!(
        repo.aws.get(&key("InRelease")).expect("stored"),
        b"old clear signed"
    );
    assert_eq!(
        repo.aws.get(&key("Release.gpg")).expect("stored"),
        b"another product's signature"
    );
    assert!(repo.aws.calls_of("create-invalidation").is_empty());
    // The packages are in the pool already: the next attempt finds them there.
    assert!(repo.aws.get("apt/pool/kbf/kbf_1.2.3_amd64.deb").is_some());
    // Without an output file the race is still a green notice.
    let repo = Repo::new("write-race-quiet", "1.2.3");
    repo.aws.before_put_of(
        "dists/stable/Release",
        Box::new(|store| {
            store.insert(key("Release"), b"changed".to_vec());
        }),
    );
    assert!(write(&repo.aws, &repo.request(false), None).is_ok());
}

#[test]
fn the_last_attempt_fails_on_a_lost_race_instead_of_asking_for_another() {
    // Catches: a fourth attempt, or a release that ends green with its index unwritten.
    let repo = Repo::new("write-last", "1.2.3");
    repo.aws.before_put_of(
        "dists/stable/InRelease",
        Box::new(|store| {
            store.insert(key("InRelease"), b"changed".to_vec());
        }),
    );
    let e = write(&repo.aws, &repo.request(true), Some(&repo.out()))
        .unwrap_err()
        .to_string();
    assert!(e.contains("apt/dists/stable/InRelease changed while the index was built, and this was the last attempt"), "{e}");
    assert_eq!(repo.written(), "");
}

#[test]
fn a_package_in_the_pool_with_the_same_bytes_is_kept_and_with_other_bytes_fails_the_write() {
    // Catches: a re-run failing on its own earlier upload, and an index listing a file that is not
    // the one in the pool.
    let repo = Repo::new("write-pool-same", "1.2.3");
    let ours = std::fs::read(asset(&repo.debs, "amd64")).expect("deb");
    repo.aws.put("apt/pool/kbf/kbf_1.2.3_amd64.deb", &ours);
    let said = write(&repo.aws, &repo.request(false), None).expect("write");
    assert!(said.contains(&"apt/pool/kbf/kbf_1.2.3_amd64.deb exists already, with the same bytes; it is kept as it is".to_owned()));
    assert!(said.contains(&"uploaded apt/pool/kbf/kbf_1.2.3_arm64.deb".to_owned()));
    assert!(!repo.dir.join("next/pool-check.deb").exists());

    let repo = Repo::new("write-pool-other", "1.2.3");
    repo.aws
        .put("apt/pool/kbf/kbf_1.2.3_amd64.deb", b"another build");
    let e = write(&repo.aws, &repo.request(false), None)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("exists already with different bytes") && e.contains("is never overwritten"),
        "{e}"
    );
    assert!(
        !repo
            .aws
            .keys_put()
            .iter()
            .any(|k| k.starts_with("apt/dists"))
    );
}

#[test]
fn a_package_that_exists_but_cannot_be_read_or_a_put_that_fails_stops_the_write() {
    // Catches: a failure of S3 taken for a lost race, and an unreadable package taken for equal.
    let repo = Repo::new("write-pool-unreadable", "1.2.3");
    repo.aws.put("apt/pool/kbf/kbf_1.2.3_amd64.deb", b"x");
    repo.aws.refuse_when("get-object", "could not connect");
    let e = write(&repo.aws, &repo.request(false), None)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("could not be read to compare it with the file to publish: could not connect"),
        "{e}"
    );

    let repo = Repo::new("write-put-fails", "1.2.3");
    repo.aws.refuse_when(
        "put-object",
        "An error occurred (AccessDenied) when calling the PutObject operation",
    );
    let e = write(&repo.aws, &repo.request(false), None)
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with(
            "put-object apt/pool/kbf/kbf_1.2.3_amd64.deb failed: An error occurred (AccessDenied)"
        ),
        "{e}"
    );
}

#[test]
fn a_failed_invalidation_fails_the_write_after_the_index_is_in_place() {
    // Catches: a CDN that keeps serving the old index while the job reports success.
    let repo = Repo::new("write-invalidate", "1.2.3");
    repo.aws
        .refuse_when("create-invalidation", "could not connect");
    let e = write(&repo.aws, &repo.request(false), Some(&repo.out()))
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "the invalidation of /apt/dists/* failed: could not connect"
    );
    assert_eq!(repo.written(), "");
}

/// `(name, damage to the prepared repository, what the refusal says)`.
type Case = (&'static str, fn(&Repo), &'static str);

#[test]
fn an_index_that_does_not_match_the_packages_or_is_incomplete_is_refused_before_any_write() {
    // Catches: an index built from other files than the ones uploaded, edited after it was built,
    // or missing a file its Release lists.
    let stable = |repo: &Repo, rel: &str| repo.dir.join("next/dists/stable").join(rel);
    let cases: [Case; 6] = [
        (
            "write-missing",
            |r| std::fs::remove_file(r.dir.join("next/dists/stable/InRelease")).expect("remove"),
            "is missing from the new index",
        ),
        (
            "write-edited",
            |r| {
                std::fs::write(
                    r.dir.join("next/dists/stable/main/binary-arm64/Packages"),
                    "edited",
                )
                .expect("edit")
            },
            "does not have the hash its Release lists",
        ),
        (
            "write-nohash",
            |r| {
                let path = r.dir.join("next/dists/stable/Release");
                let text = std::fs::read_to_string(&path).expect("release");
                let kept: String = text
                    .lines()
                    .filter(|l| !l.ends_with("binary-arm64/Packages"))
                    .map(|l| format!("{l}\n"))
                    .collect();
                std::fs::write(path, kept).expect("write");
            },
            "the Release lists no hash for main/binary-arm64/Packages",
        ),
        (
            "write-etags",
            |r| std::fs::write(r.dir.join("next/etags.txt"), "garbage").expect("write"),
            "bad line",
        ),
        (
            "write-nopackage",
            |r| std::fs::remove_file(asset(&r.debs, "arm64")).expect("remove"),
            "kbf-native-0123456789abcdef0123456789abcdef01234567-linux-arm64.deb is not in",
        ),
        (
            "write-rebuilt",
            |r| {
                let path = asset(&r.debs, "arm64");
                let mut bytes = std::fs::read(&path).expect("deb");
                bytes.push(0);
                std::fs::write(path, bytes).expect("write");
            },
            "does not list kbf_1.2.3_arm64.deb with the digest of the file to upload",
        ),
    ];
    for (name, damage, why) in cases {
        let repo = Repo::new(name, "1.2.3");
        damage(&repo);
        let e = write(&repo.aws, &repo.request(false), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{name}: {e}");
        assert!(
            repo.aws.keys_put().is_empty(),
            "{name}: nothing was written"
        );
    }
    // An index that is not UTF-8 or a package not listed at all.
    let repo = Repo::new("write-binary", "1.2.3");
    std::fs::write(stable(&repo, "Release"), [0xff, 0xfe]).expect("write");
    assert!(
        write(&repo.aws, &repo.request(false), None)
            .unwrap_err()
            .to_string()
            .contains("is not UTF-8")
    );
    let repo = Repo::new("write-unlisted", "1.2.3");
    release_files(&repo.debs, "1.2.4");
    let mut r = repo.request(false);
    r.pool = Some(Pool {
        debs: &repo.debs,
        commit: COMMIT,
        version: "1.2.4",
    });
    let e = write(&repo.aws, &r, None).unwrap_err().to_string();
    assert!(e.contains("does not list kbf_1.2.4_amd64.deb"), "{e}");
}

#[test]
fn a_by_hash_copy_that_exists_is_kept() {
    // Catches: a re-run failing because the Packages file it wrote the first time is there.
    let repo = Repo::new("write-by-hash", "1.2.3");
    let first = write(&repo.aws, &repo.request(false), None).expect("first");
    assert!(
        first
            .iter()
            .any(|l| l.starts_with("uploaded apt/dists/stable/main/binary-amd64/by-hash/SHA256/"))
    );
    // The same index again: its packages, by-hash copies are all there; the index objects are
    // conditional on the ETags that were read, which the first write replaced, so it is a lost race.
    let again = write(&repo.aws, &repo.request(false), Some(&repo.out())).expect("again");
    assert!(again.iter().any(
        |l| l.contains("by-hash/SHA256/") && l.ends_with("exists already; it is kept as it is")
    ));
    assert_eq!(repo.written(), "conflict=true\n");
}

#[test]
fn flags_out_of_their_form_are_refused() {
    // Catches: a bucket, distribution or host that is not what the settings hold.
    let repo = Repo::new("write-flags", "1.2.3");
    let before = repo.aws.calls.borrow().len();
    let bad: [fn(&mut Request); 5] = [
        |r| r.bucket = "Bad Bucket",
        |r| r.distribution_id = "",
        |r| r.distribution_id = "lower",
        |r| r.hostname = "host name",
        // A hyphen is a host name's own; the space after it is not.
        |r| r.hostname = "packages-1 komira.ai",
    ];
    for change in bad {
        let mut r = repo.request(false);
        change(&mut r);
        assert!(matches!(write(&repo.aws, &r, None), Err(Error::Usage(_))));
    }
    let mut r = repo.request(false);
    r.hostname = "";
    assert!(matches!(write(&repo.aws, &r, None), Err(Error::Usage(_))));
    assert_eq!(
        repo.aws.calls.borrow().len(),
        before,
        "nothing was asked of AWS"
    );
}
