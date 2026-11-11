use std::path::PathBuf;

use super::*;
use crate::channels::apt::build::{self, KEEP};
use crate::channels::apt::index::{parse_packages, sha256_hex};
use crate::channels::apt::read::read;
use crate::channels::apt::state::{PLATFORMS, asset_name};
use crate::channels::apt::testing::deb;
use crate::channels::apt::testing::*;
use crate::channels::apt::write::{self, Pool};
use kbf_testkit::dir::TestDir;

const ANOTHER: &str = "Package: another\nVersion: 9.9.9\nArchitecture: amd64\nSHA256: ee\nFilename: pool/another/another_9.9.9_amd64.deb\n\n";
const DATE: u64 = 1_790_000_000;

fn key(suite: &str, rel: &str) -> String {
    format!("apt/dists/{suite}/{rel}")
}

/// The signatures are text, as in the tests of `apt write`.
fn sign(next: &Path, suite: &str) {
    for (name, text) in [("Release.gpg", "signature"), ("InRelease", "clear signed")] {
        std::fs::write(next.join("dists").join(suite).join(name), text).expect("sign");
    }
}

struct Lab {
    dir: TestDir,
    debs: TestDir,
    aws: FakeAws,
}

impl Lab {
    /// A repository whose `stable` holds another product, and release `version`'s files.
    fn new(name: &str, version: &str) -> Lab {
        let dir = scratch(name);
        let debs = scratch(&format!("{name}-debs"));
        release_files(&debs, version);
        let aws = FakeAws::new();
        aws.put(
            &key("stable", "Release"),
            b"Origin: komira\nSuite: stable\nSHA256:\n aa 1 main/binary-amd64/Packages\n",
        );
        aws.put(
            &key("stable", "main/binary-amd64/Packages"),
            ANOTHER.as_bytes(),
        );
        aws.put(&key("stable", "InRelease"), b"old clear signed");
        Lab { dir, debs, aws }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// Publishes release `version` from `debs` into `suite`, as the release's jobs do: read,
    /// index, sign, write.
    fn publish(&self, suite: Suite, tag: &str, debs: &Path, version: &str) -> Vec<String> {
        let (current, next) = (
            self.path(&format!("{tag}-current")),
            self.path(&format!("{tag}-next")),
        );
        read(&self.aws, suite, BUCKET, &current).expect("read");
        build::build(&build::Request {
            suite,
            current: &current,
            out: &next,
            debs,
            commit: COMMIT,
            version,
            origin: "komira",
            label: "komira",
            description: "komira packages",
            date: DATE,
            keep: KEEP,
        })
        .expect("build");
        sign(&next, suite.name());
        write::write(
            &self.aws,
            &write::Request {
                suite,
                bucket: BUCKET,
                distribution_id: "E123ABC",
                hostname: "packages.komira.ai",
                next: &next,
                pool: Some(Pool {
                    debs,
                    commit: COMMIT,
                    version,
                }),
                last: true,
            },
            None,
        )
        .expect("write")
    }

    /// The `SHA256SUMS` of the release files in `debs`.
    fn sums(&self, debs: &Path, version: &str) -> PathBuf {
        let mut text = String::new();
        for (arch, platform) in PLATFORMS {
            let name = asset_name(COMMIT, platform);
            let bytes = std::fs::read(asset(debs, arch)).expect("deb");
            text.push_str(&format!("{}  {name}\n", sha256_hex(&bytes)));
        }
        let path = self.path(&format!("SHA256SUMS-{version}"));
        std::fs::write(&path, text).expect("sums");
        path
    }

    /// Reads both suites and promotes `version`, as the promote job does; `tag` names the
    /// directories.
    fn promote(&self, tag: &str, version: &str, sums: &Path) -> Result<Vec<String>, Error> {
        let (testing, stable, out) = (
            self.path(&format!("{tag}-testing")),
            self.path(&format!("{tag}-stable")),
            self.path(&format!("{tag}-promoted")),
        );
        read(&self.aws, Suite::Testing, BUCKET, &testing).expect("read testing");
        read(&self.aws, Suite::Stable, BUCKET, &stable).expect("read stable");
        promote(&Request {
            testing: &testing,
            stable: &stable,
            out: &out,
            version,
            sums,
            commit: COMMIT,
            origin: "komira",
            label: "komira",
            description: "komira packages",
            date: DATE,
            keep: KEEP,
        })
    }

    /// Writes the promoted index of `tag` into `stable`, index only.
    fn write_stable(&self, tag: &str) -> Vec<String> {
        let next = self.path(&format!("{tag}-promoted"));
        sign(&next, "stable");
        write::write(
            &self.aws,
            &write::Request {
                suite: Suite::Stable,
                bucket: BUCKET,
                distribution_id: "E123ABC",
                hostname: "packages.komira.ai",
                next: &next,
                pool: None,
                last: true,
            },
            None,
        )
        .expect("write stable")
    }

    fn listed(&self, tag: &str) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for arch in ["amd64", "arm64"] {
            let path = self.path(&format!(
                "{tag}-promoted/dists/stable/main/binary-{arch}/Packages"
            ));
            for s in
                parse_packages(&std::fs::read_to_string(path).expect("packages")).expect("parse")
            {
                out.push((
                    s.package,
                    format!("{}.{}.{}", s.version.0, s.version.1, s.version.2),
                    s.sha256,
                ));
            }
        }
        out
    }
}

#[test]
fn a_release_published_to_testing_writes_the_testing_suite_and_the_pool_and_leaves_stable_alone() {
    // Catches: a publish to `testing` that writes under `dists/stable/`, or that names the
    // suite `stable` in its Release.
    let lab = Lab::new("promote-testing-only", "1.2.3");
    let before: Vec<(String, Vec<u8>)> = lab
        .aws
        .store
        .borrow()
        .iter()
        .filter(|(k, _)| k.starts_with("apt/dists/stable/"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let said = lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    assert!(
        said.last()
            .expect("a line")
            .ends_with("/apt/dists/testing/InRelease"),
        "{said:?}"
    );
    let keys = lab.aws.keys_put();
    assert!(
        keys.iter()
            .all(|k| k.starts_with("apt/dists/testing/") || k.starts_with("apt/pool/kbf/")),
        "{keys:?}"
    );
    assert!(keys.contains(&"apt/pool/kbf/kbf_1.2.3_amd64.deb".to_owned()));
    let after: Vec<(String, Vec<u8>)> = lab
        .aws
        .store
        .borrow()
        .iter()
        .filter(|(k, _)| k.starts_with("apt/dists/stable/"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(before, after);
    let release =
        String::from_utf8(lab.aws.get(&key("testing", "Release")).expect("release")).expect("text");
    assert!(
        release.contains("Suite: testing\nCodename: testing\n"),
        "{release}"
    );
    assert!(!release.contains("stable"), "{release}");
}

#[test]
fn testing_and_stable_never_share_an_object_key() {
    // Catches: a path that does not take the suite, so one suite's write replaces the other's.
    for rel in ["Release", "InRelease", "main/binary-amd64/Packages.gz"] {
        assert_ne!(
            crate::channels::apt::state::index_key(Suite::Testing, rel),
            crate::channels::apt::state::index_key(Suite::Stable, rel)
        );
        assert_ne!(
            crate::channels::apt::state::by_hash_key(Suite::Testing, rel, "ab"),
            crate::channels::apt::state::by_hash_key(Suite::Stable, rel, "ab")
        );
    }
}

#[test]
fn a_promotion_lists_the_testing_entries_in_stable_with_no_package_written_and_keeps_other_products()
 {
    // Catches: a promotion that uploads packages, that lists other bytes than testing holds, or
    // that drops another product's entry from stable.
    let lab = Lab::new("promote-writes-index-only", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    let said = lab.promote("p", "1.2.3", &sums).expect("promote");
    assert_eq!(said, ["apt promote: added 2, dropped 0, listed 3"]);
    let listed = lab.listed("p");
    let digest = |arch: &str| sha256_hex(&std::fs::read(asset(&lab.debs, arch)).expect("deb"));
    assert_eq!(
        listed,
        [
            ("another".to_owned(), "9.9.9".to_owned(), "ee".to_owned()),
            ("kbf".to_owned(), "1.2.3".to_owned(), digest("amd64")),
            ("kbf".to_owned(), "1.2.3".to_owned(), digest("arm64")),
        ]
    );
    let puts_before = lab.aws.keys_put().len();
    let wrote = lab.write_stable("p");
    let keys: Vec<String> = lab.aws.keys_put().split_off(puts_before);
    assert!(
        keys.iter().all(|k| k.starts_with("apt/dists/stable/")),
        "{keys:?}"
    );
    assert!(keys.contains(&key("stable", "InRelease")));
    assert!(
        wrote
            .last()
            .expect("a line")
            .ends_with("/apt/dists/stable/InRelease"),
        "{wrote:?}"
    );
    // The testing suite is as the publish left it.
    let release =
        String::from_utf8(lab.aws.get(&key("stable", "Release")).expect("release")).expect("text");
    assert!(
        release.contains("Suite: stable\nCodename: stable\n"),
        "{release}"
    );
    assert!(release.starts_with("Origin: komira\n"), "{release}");
}

#[test]
fn promoting_a_version_twice_writes_the_same_index_and_changes_no_entry() {
    // Catches: a re-run of the promotion listing the version twice or moving an entry.
    let lab = Lab::new("promote-twice", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    lab.promote("one", "1.2.3", &sums).expect("first");
    lab.write_stable("one");
    let said = lab.promote("two", "1.2.3", &sums).expect("second");
    assert_eq!(said, ["apt promote: added 0, dropped 0, listed 3"]);
    assert_eq!(lab.listed("one"), lab.listed("two"));
    for rel in [
        "main/binary-amd64/Packages",
        "main/binary-arm64/Packages.gz",
        "Release",
    ] {
        let file = |tag: &str| {
            std::fs::read(lab.path(&format!("{tag}-promoted/dists/stable/{rel}"))).expect("file")
        };
        assert_eq!(file("one"), file("two"), "{rel}");
    }
}

#[test]
fn a_version_testing_does_not_list_is_refused_and_nothing_is_written() {
    // Catches: promoting a release that never reached testing.
    let lab = Lab::new("promote-not-in-testing", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    release_files(&lab.debs, "1.2.4");
    let sums = lab.sums(&lab.debs, "1.2.4");
    let e = lab.promote("p", "1.2.4", &sums).unwrap_err().to_string();
    assert!(
        e.contains("testing does not list kbf_1.2.4_amd64.deb"),
        "{e}"
    );
    assert!(!lab.path("p-promoted").exists());
}

#[test]
fn an_entry_whose_digest_is_not_the_one_in_sha256sums_is_refused() {
    // Catches: promoting bytes other than the ones the release was verified against.
    let lab = Lab::new("promote-bad-digest", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    let text = std::fs::read_to_string(&sums).expect("sums");
    let other = sha256_hex(b"other bytes");
    let forged: String = text
        .lines()
        .map(|l| match l.split_once("  ") {
            Some((_, name)) if name.ends_with("arm64.deb") => format!("{other}  {name}\n"),
            _ => format!("{l}\n"),
        })
        .collect();
    std::fs::write(&sums, forged).expect("forge");
    let e = lab.promote("p", "1.2.3", &sums).unwrap_err().to_string();
    assert!(
        e.contains(&format!(
            "testing lists kbf_1.2.3_arm64.deb with sha256 {}, but SHA256SUMS lists {other}",
            sha256_hex(&std::fs::read(asset(&lab.debs, "arm64")).expect("deb"))
        )),
        "{e}"
    );
    assert!(!lab.path("p-promoted").exists());
}

#[test]
fn sums_that_do_not_list_a_package_or_cannot_be_read_refuse_the_promotion() {
    // Catches: a promotion with no digest to hold the entries to.
    let lab = Lab::new("promote-no-sums", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    let text = std::fs::read_to_string(&sums).expect("sums");
    let without: String = text
        .lines()
        .filter(|l| !l.ends_with("arm64.deb"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&sums, without).expect("write");
    let e = lab.promote("p", "1.2.3", &sums).unwrap_err().to_string();
    assert!(
        e.contains(&format!(
            "SHA256SUMS lists no kbf-native-{COMMIT}-linux-arm64.deb"
        )),
        "{e}"
    );
    assert!(
        lab.promote("q", "1.2.3", &lab.path("missing-sums"))
            .is_err()
    );
}

#[test]
fn a_stable_that_is_newer_or_lists_the_version_with_other_bytes_is_refused() {
    // Catches: a promotion that moves the stable channel down, or replaces an entry's bytes.
    let lab = Lab::new("promote-stable-ahead", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    // Stable lists 1.2.4, newer than the version to promote.
    let newer = scratch("promote-stable-ahead-newer");
    release_files(&newer, "1.2.4");
    lab.publish(Suite::Stable, "s", &newer, "1.2.4");
    let e = lab.promote("p", "1.2.3", &sums).unwrap_err().to_string();
    assert!(
        e.contains("is older than the newest listed, which would move the channel down"),
        "{e}"
    );

    // Stable lists 1.2.3 with other bytes than testing.
    let lab = Lab::new("promote-stable-other-bytes", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    let other = scratch("promote-stable-other-bytes-debs");
    for arch in ["amd64", "arm64"] {
        let files = deb::files(arch.as_bytes(), b"another notice");
        let bytes = deb::build("1.2.3", arch, &files);
        std::fs::write(asset(&other, arch), bytes).expect("write");
    }
    // The pool already holds the testing bytes, so the stable index is built directly.
    let current = lab.path("s-current");
    read(&lab.aws, Suite::Stable, BUCKET, &current).expect("read");
    build::build(&build::Request {
        suite: Suite::Stable,
        current: &current,
        out: &lab.path("s-next"),
        debs: &other,
        commit: COMMIT,
        version: "1.2.3",
        origin: "komira",
        label: "komira",
        description: "komira packages",
        date: DATE,
        keep: KEEP,
    })
    .expect("build");
    for rel in [
        "Release",
        "main/binary-amd64/Packages",
        "main/binary-arm64/Packages",
        "main/binary-amd64/Packages.gz",
        "main/binary-arm64/Packages.gz",
    ] {
        let bytes = std::fs::read(lab.path(&format!("s-next/dists/stable/{rel}"))).expect("file");
        lab.aws.put(&key("stable", rel), &bytes);
    }
    let e = lab.promote("p", "1.2.3", &sums).unwrap_err().to_string();
    assert!(
        e.contains("is listed at this version with other bytes"),
        "{e}"
    );
}

#[test]
fn a_stable_that_was_not_read_and_a_version_out_of_its_form_are_errors() {
    // Catches: a promotion built on no ETags (its write could not be conditional), and a version
    // that is not a version.
    let lab = Lab::new("promote-usage", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    let (testing, out, nowhere) = (lab.path("t-current"), lab.path("o"), lab.path("nowhere"));
    let request = |version| {
        promote(&Request {
            testing: &testing,
            stable: &nowhere,
            out: &out,
            version,
            sums: &sums,
            commit: COMMIT,
            origin: "komira",
            label: "komira",
            description: "komira packages",
            date: DATE,
            keep: KEEP,
        })
    };
    let e = request("1.2.3").unwrap_err().to_string();
    assert!(
        e.contains("etags.txt is missing: run `apt read --suite stable` first"),
        "{e}"
    );
    assert!(matches!(request("v1.2.3"), Err(Error::Usage(_))));
}

#[test]
fn a_write_without_a_pool_needs_no_packages_and_with_one_needs_them() {
    // Catches: `--no-pool` still asking for the .debs, or a pool write that skips them.
    let lab = Lab::new("promote-no-pool", "1.2.3");
    lab.publish(Suite::Testing, "t", &lab.debs, "1.2.3");
    let sums = lab.sums(&lab.debs, "1.2.3");
    lab.promote("p", "1.2.3", &sums).expect("promote");
    lab.write_stable("p");
    let next = lab.path("p-promoted");
    let with_pool = write::Request {
        suite: Suite::Stable,
        bucket: BUCKET,
        distribution_id: "E123ABC",
        hostname: "packages.komira.ai",
        next: &next,
        pool: Some(Pool {
            debs: &lab.path("no-debs"),
            commit: COMMIT,
            version: "1.2.3",
        }),
        last: true,
    };
    let e = write::write(&lab.aws, &with_pool, None)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!("kbf-native-{COMMIT}-linux-x86_64.deb is not in")),
        "{e}"
    );
}

#[test]
fn a_commit_that_is_not_a_full_commit_is_a_usage_error_before_anything_is_read() {
    // Catches: a short or upper case commit naming no asset in SHA256SUMS, found only as "lists no".
    let lab = Lab::new("promote-bad-commit", "1.2.3");
    let e = promote(&Request {
        testing: &lab.path("t"),
        stable: &lab.path("s"),
        out: &lab.path("o"),
        commit: "0123456789ab",
        version: "1.2.3",
        sums: &lab.path("SHA256SUMS"),
        origin: "komira",
        label: "komira",
        description: "komira packages",
        date: DATE,
        keep: KEEP,
    })
    .unwrap_err();
    assert!(
        matches!(&e, Error::Usage(why) if why.starts_with("--commit \"0123456789ab\" is not a full commit")),
        "{e}"
    );
}
