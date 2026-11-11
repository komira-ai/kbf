use super::*;
use crate::channels::apt::control;
use crate::channels::apt::index::{parse_packages, sha256_hex};
use crate::channels::apt::read::read;
use crate::channels::apt::testing::*;

const OTHER: &str = "Package: another\nVersion: 9.9.9\nArchitecture: amd64\nSHA256: ee\nFilename: pool/another/another_9.9.9_amd64.deb\nDescription: other\n";

/// A repository that another product published into: its `Release`, `Packages` of amd64 and of
/// `all`, read into `<dir>/current`.
fn shared_repository(dir: &Path, aws: &FakeAws) {
    let key = |rel: &str| format!("apt/dists/stable/{rel}");
    aws.put(
        &key("Release"),
        b"Origin: komira\nSuite: stable\nSHA256:\n aa 1 main/binary-amd64/Packages\n bb 1 main/binary-all/Packages\n",
    );
    aws.put(
        &key("main/binary-amd64/Packages"),
        format!("{OTHER}\n").as_bytes(),
    );
    aws.put(
        &key("main/binary-all/Packages"),
        b"Package: docs\nVersion: 1.0.0\nArchitecture: all\nSHA256: dd\n\n",
    );
    read(aws, Suite::Stable, BUCKET, &dir.join("current")).expect("read");
}

fn request<'a>(dir: &'a Path, debs: &'a Path) -> Request<'a> {
    Request {
        suite: Suite::Stable,
        current: Box::leak(dir.join("current").into_boxed_path()),
        out: Box::leak(dir.join("next").into_boxed_path()),
        debs,
        commit: COMMIT,
        version: "1.2.3",
        origin: "komira",
        label: "komira",
        description: "komira packages",
        date: 1_790_000_000,
        keep: KEEP,
    }
}

fn packages(dir: &Path, arch: &str) -> Vec<(String, String)> {
    let text =
        std::fs::read_to_string(dir.join(format!("next/dists/stable/main/binary-{arch}/Packages")))
            .expect("packages");
    parse_packages(&text)
        .expect("parse")
        .into_iter()
        .map(|s| {
            (
                s.package,
                format!("{}.{}.{}", s.version.0, s.version.1, s.version.2),
            )
        })
        .collect()
}

#[test]
fn the_packages_join_a_shared_index_and_every_other_entry_and_architecture_stays() {
    // Catches: kbf's publish dropping another product's package, an architecture another
    // product lists, or that repository's own Origin.
    let dir = scratch("build-shared");
    let aws = FakeAws::new();
    shared_repository(&dir, &aws);
    let debs = scratch("build-shared-debs");
    release_files(&debs, "1.2.3");
    let said = build(&request(&dir, &debs)).expect("build");
    assert_eq!(said, ["apt index: added 2, dropped 0, listed 4"]);
    assert_eq!(
        packages(&dir, "amd64"),
        [
            ("another".to_owned(), "9.9.9".to_owned()),
            ("kbf".to_owned(), "1.2.3".to_owned())
        ]
    );
    assert_eq!(
        packages(&dir, "arm64"),
        [("kbf".to_owned(), "1.2.3".to_owned())]
    );
    assert_eq!(
        packages(&dir, "all"),
        [("docs".to_owned(), "1.0.0".to_owned())]
    );
    let release = std::fs::read_to_string(dir.join("next/dists/stable/Release")).expect("release");
    assert!(
        release.starts_with("Origin: komira\nSuite: stable\n"),
        "{release}"
    );
    assert!(release.contains("Architectures: all amd64 arm64\n"));
    assert!(release.contains("Date: Mon, 21 Sep 2026 14:13:20 UTC\n"));
    assert!(!release.contains("Label"));
    // The ETags the index was built from travel with it.
    assert_eq!(
        std::fs::read(dir.join("next/etags.txt")).expect("next"),
        std::fs::read(dir.join("current/etags.txt")).expect("current")
    );
    // The stanza lists the file the job will upload, by its digest.
    let amd = std::fs::read(asset(&debs, "amd64")).expect("deb");
    let text = std::fs::read_to_string(dir.join("next/dists/stable/main/binary-amd64/Packages"))
        .expect("packages");
    let ours = parse_packages(&text).expect("parse").pop().expect("ours");
    assert_eq!(ours.sha256, sha256_hex(&amd));
    let fields = control::parse(&ours.text).expect("fields");
    assert_eq!(
        control::get(&fields, "Filename"),
        Some("pool/kbf/kbf_1.2.3_amd64.deb")
    );
}

#[test]
fn a_first_release_takes_its_origin_label_and_description_from_the_flags() {
    // Catches: a repository created without its identity.
    let dir = scratch("build-first");
    let aws = FakeAws::new();
    read(&aws, Suite::Stable, BUCKET, &dir.join("current")).expect("read");
    let debs = scratch("build-first-debs");
    release_files(&debs, "1.2.3");
    build(&request(&dir, &debs)).expect("build");
    let release = std::fs::read_to_string(dir.join("next/dists/stable/Release")).expect("release");
    assert!(release.starts_with("Origin: komira\nLabel: komira\nSuite: stable\n"));
    assert!(release.contains("Description: komira packages\n"));
    assert_eq!(
        packages(&dir, "amd64"),
        [("kbf".to_owned(), "1.2.3".to_owned())]
    );
}

#[test]
fn a_re_run_of_the_same_release_lists_it_once() {
    // Catches: a second attempt after a lost race adding a second entry.
    let dir = scratch("build-twice");
    let aws = FakeAws::new();
    read(&aws, Suite::Stable, BUCKET, &dir.join("current")).expect("read");
    let debs = scratch("build-twice-debs");
    release_files(&debs, "1.2.3");
    build(&request(&dir, &debs)).expect("first");
    for rel in ["Packages"] {
        for arch in ["amd64", "arm64"] {
            let from = dir.join(format!("next/dists/stable/main/binary-{arch}/{rel}"));
            let to = dir.join(format!("current/dists/stable/main/binary-{arch}/{rel}"));
            std::fs::create_dir_all(to.parent().expect("dir")).expect("dir");
            std::fs::copy(from, to).expect("copy");
        }
    }
    let said = build(&request(&dir, &debs)).expect("second");
    assert_eq!(said, ["apt index: added 0, dropped 0, listed 2"]);
}

#[test]
fn a_release_older_than_the_newest_listed_or_with_other_bytes_writes_nothing() {
    // Catches: the channel moving down, a rebuilt package replacing a listed one, and a half-written
    // index left behind by a refusal.
    for (listed, why) in [("2.0.0", "move the channel down"), ("1.2.3", "other bytes")] {
        let dir = scratch("build-refused");
        let aws = FakeAws::new();
        aws.put(
            "apt/dists/stable/main/binary-amd64/Packages",
            format!("Package: kbf\nVersion: {listed}\nArchitecture: amd64\nSHA256: 00\n\n")
                .as_bytes(),
        );
        read(&aws, Suite::Stable, BUCKET, &dir.join("current")).expect("read");
        let debs = scratch("build-refused-debs");
        release_files(&debs, "1.2.3");
        let e = build(&request(&dir, &debs)).unwrap_err().to_string();
        assert!(e.contains(why), "{e}");
        assert!(!dir.join("next").exists());
    }
}

#[test]
fn what_cannot_be_built_is_refused_with_the_reason() {
    // Catches: an index built without its read, from a package missing or not what its name says,
    // or from an index file that is not one.
    let dir = scratch("build-bad");
    let aws = FakeAws::new();
    let debs = scratch("build-bad-debs");
    release_files(&debs, "1.2.3");
    let e = build(&request(&dir, &debs)).unwrap_err().to_string();
    assert!(
        e.contains("etags.txt is missing: run `apt read` first"),
        "{e}"
    );
    read(&aws, Suite::Stable, BUCKET, &dir.join("current")).expect("read");
    // The release files of version 1.2.4 asked for as 1.2.3: their control files do not say the name.
    let other = scratch("build-bad-other");
    release_files(&other, "1.2.4");
    let e = build(&request(&dir, &other)).unwrap_err().to_string();
    assert!(e.contains("does not say the name it has"), "{e}");
    // Not a package.
    std::fs::write(asset(&other, "amd64"), "junk").expect("junk");
    let e = build(&request(&dir, &other)).unwrap_err().to_string();
    assert!(
        e.starts_with("kbf_1.2.3_amd64.deb: not a Debian package"),
        "{e}"
    );
    // A package that is not there.
    std::fs::remove_file(asset(&other, "amd64")).expect("remove");
    assert!(build(&request(&dir, &other)).is_err());
    // An index file that is not one.
    let packages = dir.join("current/dists/stable/main/binary-amd64");
    std::fs::create_dir_all(&packages).expect("dir");
    std::fs::write(packages.join("Packages"), "garbage\n").expect("write");
    let e = build(&request(&dir, &debs)).unwrap_err().to_string();
    assert!(
        e.contains("binary-amd64/Packages: control line without a colon"),
        "{e}"
    );
    std::fs::write(packages.join("Packages"), "").expect("write");
    // A release that is not control text.
    std::fs::write(dir.join("current/dists/stable/Release"), "no colon here").expect("write");
    assert!(build(&request(&dir, &debs)).is_err());
    // A keep of 0.
    std::fs::remove_file(dir.join("current/dists/stable/Release")).expect("remove");
    let mut r = request(&dir, &debs);
    r.keep = 0;
    assert!(matches!(build(&r), Err(Error::Usage(_))));
}

#[test]
fn an_out_directory_that_cannot_be_written_is_an_error_naming_it() {
    // Catches: a build that reports success with nothing on disk.
    let dir = scratch("build-disk");
    let aws = FakeAws::new();
    read(&aws, Suite::Stable, BUCKET, &dir.join("current")).expect("read");
    let debs = scratch("build-disk-debs");
    release_files(&debs, "1.2.3");
    std::fs::write(dir.join("next"), "a file").expect("file");
    let e = build(&request(&dir, &debs)).unwrap_err().to_string();
    assert!(e.starts_with("write "), "{e}");
}
