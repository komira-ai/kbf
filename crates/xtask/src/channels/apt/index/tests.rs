use super::*;
use crate::channels::apt::testing::deb;

// The goldens are what the repository's other publisher writes for one of its packages at 1.2.2
// and 1.2.3: the text of `Packages` and of `Release` this module must write for the same
// stanzas. Only the gzip lines of the `Release` differ: the compressor
// is not the same program.
const PACKAGES_AMD64: &str = include_str!("golden/Packages_amd64");
const PACKAGES_ARM64: &str = include_str!("golden/Packages_arm64");
const RELEASE: &str = include_str!("golden/Release");

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn deb_of(version: &str, arch: &str) -> Vec<u8> {
    deb::build(version, arch, &deb::files(b"binary", b"notice"))
}

fn stanza(version: &str, arch: &str, sha: &str) -> Stanza {
    from_fields(&format!(
        "Package: kbf\nVersion: {version}\nArchitecture: {arch}\nSHA256: {sha}\n"
    ))
    .expect("stanza")
}

fn versions(merged: &Merged) -> Vec<String> {
    merged
        .stanzas
        .iter()
        .map(|s| {
            format!(
                "{}.{}.{}/{}",
                s.version.0, s.version.1, s.version.2, s.architecture
            )
        })
        .collect()
}

#[test]
fn the_packages_and_release_text_is_what_the_other_repository_writes() {
    // Catches: a format drift between this tool and the other publisher, which feed one repository.
    let amd = parse_packages(PACKAGES_AMD64).expect("amd64");
    let arm = parse_packages(PACKAGES_ARM64).expect("arm64");
    assert_eq!((amd.len(), arm.len()), (2, 1));
    let (new_amd, old_amd) = (amd[0].clone(), amd[1].clone());
    let first = merge(&[], &[old_amd], 25).expect("first");
    let second = merge(&first.stanzas, &[new_amd, arm[0].clone()], 25).expect("second");
    assert_eq!(second.added, 2);
    let info = ReleaseInfo {
        origin: Some("buildrules".to_owned()),
        label: Some("buildrules".to_owned()),
        description: Some("buildrules packages".to_owned()),
        date: 1_790_000_000,
    };
    let files = tree(
        Suite::Stable,
        &info,
        &second.stanzas,
        &strings(&["arm64", "amd64"]),
    )
    .expect("tree");
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "dists/stable/main/binary-amd64/Packages",
            "dists/stable/main/binary-amd64/Packages.gz",
            "dists/stable/main/binary-arm64/Packages",
            "dists/stable/main/binary-arm64/Packages.gz",
            "dists/stable/Release"
        ]
    );
    assert_eq!(files[0].data, PACKAGES_AMD64.as_bytes());
    assert_eq!(files[2].data, PACKAGES_ARM64.as_bytes());
    // Every line of the Release but the two gzip lines is the golden's; the gzip lines list
    // the hash and size of the file written.
    let release = String::from_utf8(files[4].data.clone()).expect("utf8");
    let plain = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| !l.ends_with(".gz"))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(plain(&release), plain(RELEASE));
    for file in [&files[1], &files[3]] {
        let rel = file
            .path
            .strip_prefix("dists/stable/")
            .expect("below stable");
        let want = format!(" {} {} {rel}", sha256_hex(&file.data), file.data.len());
        assert!(release.lines().any(|l| l == want), "{want}");
    }
}

#[test]
fn the_stanza_of_a_package_names_its_pool_file_its_size_and_the_sha256_of_its_bytes() {
    // Catches: an index entry that does not match the file, or a Description that is not last.
    let bytes = deb_of("1.2.3", "amd64");
    let s = stanza_of(&bytes).expect("stanza");
    assert_eq!(
        (s.package.as_str(), s.version, s.architecture.as_str()),
        ("kbf", (1, 2, 3), "amd64")
    );
    assert_eq!(s.sha256, sha256_hex(&bytes));
    let fields = control::parse(&s.text).expect("parse");
    assert_eq!(
        control::get(&fields, "Filename"),
        Some("pool/kbf/kbf_1.2.3_amd64.deb")
    );
    assert_eq!(
        control::get(&fields, "Size"),
        Some(bytes.len().to_string().as_str())
    );
    assert_eq!(fields[0].name, "Package");
    assert_eq!(fields.last().expect("last").name, "Description");
    assert!(s.text.ends_with('\n'));
}

/// A package whose control file is `control`.
fn deb_with_control(control: &str) -> Vec<u8> {
    let tar = {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(control.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "./control", control.as_bytes())
            .expect("append");
        builder.into_inner().expect("tar")
    };
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, &tar).expect("gz");
    let gz = gz.finish().expect("finish");
    deb::ar(&[("debian-binary", b"2.0\n"), ("control.tar.gz", &gz)])
}

#[test]
fn a_package_that_would_forge_its_entry_or_leave_the_pool_is_refused() {
    // Catches: a control file naming its own Filename, SHA256 or Size, a name or architecture
    // that is a path, a version that is not X.Y.Z, and a control file missing a field.
    let base = "Package: kbf\nVersion: 1.2.3\nArchitecture: amd64\n";
    assert!(stanza_of(&deb_with_control(base)).is_ok());
    // Debian's other name characters are names.
    assert!(
        stanza_of(&deb_with_control(
            "Package: lib2-tier.store+x\nVersion: 1.2.3\nArchitecture: all\n"
        ))
        .is_ok()
    );
    for (control, why) in [
        (format!("{base}sha256: forged\n"), "writes itself"),
        (format!("{base}Filename: ../../x\n"), "writes itself"),
        (
            "Package: ../x\nVersion: 1.2.3\nArchitecture: amd64\n".to_owned(),
            "cannot be a file",
        ),
        (
            "Package: kbf\nVersion: 1.2\nArchitecture: amd64\n".to_owned(),
            "cannot be a file",
        ),
        (
            "Package: kbf\nVersion: 1.2.3\nArchitecture: a/b\n".to_owned(),
            "cannot be a file",
        ),
        (
            "Package: kbf\nVersion: 1.2.3\n".to_owned(),
            "no Architecture",
        ),
        (
            "Package: A\nVersion: 1.2.3\nArchitecture: amd64\n".to_owned(),
            "cannot be a file",
        ),
    ] {
        let e = stanza_of(&deb_with_control(&control))
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{control:?}: {e}");
    }
    assert!(stanza_of(b"junk").is_err());
}

#[test]
fn a_packages_stanza_needs_its_name_version_architecture_and_checksum() {
    // Catches: an index file from another product that is half-read.
    for text in [
        "Version: 1.2.3\nArchitecture: amd64\nSHA256: a\n",
        "Package: p\nVersion: 1.2\nArchitecture: amd64\nSHA256: a\n",
        "Package: p\nVersion: 1.2.3\nArchitecture: amd64\n",
        "garbage\n",
    ] {
        assert!(parse_packages(text).is_err(), "{text:?}");
    }
    assert!(parse_packages("").expect("empty").is_empty());
}

#[test]
fn new_versions_are_added_newest_first_and_the_same_file_again_is_a_no_op() {
    // Catches: 1.10.0 sorted before 1.9.0 as text, a re-run adding a second entry, and an
    // architecture mixed into another's slot.
    let one = merge(&[], &[stanza("1.9.0", "amd64", "a")], 25).expect("one");
    assert_eq!((one.added, one.dropped), (1, 0));
    let two = merge(
        &one.stanzas,
        &[
            stanza("1.10.0", "amd64", "b"),
            stanza("1.0.0", "arm64", "c"),
        ],
        25,
    )
    .expect("two");
    assert_eq!(
        versions(&two),
        ["1.10.0/amd64", "1.9.0/amd64", "1.0.0/arm64"]
    );
    let again = merge(&two.stanzas, &[stanza("1.10.0", "amd64", "b")], 25).expect("again");
    assert_eq!((again.added, again.stanzas), (0, two.stanzas));
}

#[test]
fn an_older_version_or_other_bytes_for_a_listed_version_are_refused() {
    // Catches: the channel moving down, and a rebuilt file replacing a listed one in the index.
    let listed = merge(&[], &[stanza("1.2.3", "amd64", "a")], 25)
        .expect("listed")
        .stanzas;
    let down = merge(&listed, &[stanza("1.2.2", "amd64", "x")], 25).unwrap_err();
    assert!(down.to_string().contains("move the channel down"));
    let other = merge(&listed, &[stanza("1.2.3", "amd64", "x")], 25).unwrap_err();
    assert!(other.to_string().contains("other bytes"));
    // Another architecture's older version is a different slot.
    assert!(merge(&listed, &[stanza("1.0.0", "arm64", "x")], 25).is_ok());
    assert!(merge(&listed, &[], 0).is_err());
}

#[test]
fn only_the_newest_keep_versions_of_a_slot_stay_listed() {
    // Catches: an index that grows with every release, and one that drops the newest.
    let mut stanzas = Vec::new();
    for v in ["1.0.0", "1.1.0", "1.2.0"] {
        stanzas = merge(&stanzas, &[stanza(v, "amd64", v)], 25)
            .expect("merge")
            .stanzas;
    }
    stanzas.push(stanza("1.0.0", "arm64", "arm"));
    let kept = merge(&stanzas, &[stanza("1.3.0", "amd64", "n")], 2).expect("keep");
    assert_eq!(
        versions(&kept),
        ["1.3.0/amd64", "1.2.0/amd64", "1.0.0/arm64"]
    );
    assert_eq!((kept.added, kept.dropped), (1, 2));
}

#[test]
fn another_products_entries_are_kept_untouched() {
    // Catches: the shared index losing a product when kbf publishes.
    let other = from_fields(
        "Package: another\nVersion: 9.9.9\nArchitecture: amd64\nSHA256: z\nDescription: x\n",
    )
    .expect("other");
    let merged = merge(
        std::slice::from_ref(&other),
        &[stanza("1.0.0", "amd64", "a")],
        25,
    )
    .expect("merge");
    assert_eq!(merged.stanzas[0], other);
    assert_eq!(merged.stanzas[1].package, "kbf");
}

#[test]
fn the_release_of_a_shared_repository_keeps_its_own_origin_label_and_description() {
    // Catches: kbf renaming a repository other products publish into.
    let info = ReleaseInfo {
        origin: Some("o".to_owned()),
        label: Some("l".to_owned()),
        description: Some("d".to_owned()),
        date: 5,
    };
    assert_eq!(keep_shared_fields(None, info.clone()).expect("none"), info);
    assert_eq!(
        keep_shared_fields(Some(""), info.clone()).expect("empty"),
        info
    );
    let kept =
        keep_shared_fields(Some("Origin: komira\nSuite: stable\n"), info.clone()).expect("kept");
    assert_eq!(
        kept,
        ReleaseInfo {
            origin: Some("komira".to_owned()),
            label: None,
            description: None,
            date: 5
        }
    );
    assert!(keep_shared_fields(Some("no colon"), info).is_err());
}

#[test]
fn a_release_without_a_label_or_description_omits_them() {
    // Catches: empty Label or Description lines that a control file cannot hold.
    let info = ReleaseInfo {
        origin: None,
        label: None,
        description: None,
        date: 0,
    };
    let files = tree(Suite::Stable, &info, &[], &strings(&["amd64"])).expect("tree");
    let release = String::from_utf8(files[2].data.clone()).expect("utf8");
    assert!(release.starts_with("Suite: stable\nCodename: stable\nDate: Thu, 01 Jan 1970 00:00:00 UTC\nArchitectures: amd64\nComponents: main\nAcquire-By-Hash: yes\nSHA256:\n "));
    // An empty index is a Packages of no bytes and its gzip.
    assert!(files[0].data.is_empty() && !files[1].data.is_empty());
}

#[test]
fn an_architecture_that_is_not_a_name_or_is_listed_twice_or_a_stanza_without_one_is_refused() {
    // Catches: a Packages written under a directory the Release does not list.
    let info = ReleaseInfo {
        origin: None,
        label: None,
        description: None,
        date: 0,
    };
    assert!(tree(Suite::Stable, &info, &[], &strings(&["a/b"])).is_err());
    assert!(tree(Suite::Stable, &info, &[], &strings(&["amd64", "amd64"])).is_err());
    let e = tree(
        Suite::Stable,
        &info,
        &[stanza("1.0.0", "s390x", "a")],
        &strings(&["amd64"]),
    )
    .unwrap_err();
    assert!(e.to_string().contains("s390x"));
}

#[test]
fn the_date_is_rfc_2822_in_utc() {
    // Catches: a Date apt rejects: a wrong weekday, month or leap day.
    assert_eq!(format_date(0), "Thu, 01 Jan 1970 00:00:00 UTC");
    assert_eq!(format_date(1_790_000_000), "Mon, 21 Sep 2026 14:13:20 UTC");
    assert_eq!(format_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 UTC");
    assert_eq!(format_date(1_798_761_599), "Thu, 31 Dec 2026 23:59:59 UTC");
    assert_eq!(format_date(1_798_761_600), "Fri, 01 Jan 2027 00:00:00 UTC");
}
