//! `cargo xtask channels apt index` and `... apt promote` through the built program: the command
//! line is parsed and run against scratch directories, and what is checked is the exit status
//! and the files written. Nothing here reaches a network, a cloud or a signing key.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::Compression;
use kbf_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const VERSION: &str = "1.2.3";
const ARCHS: [(&str, &str); 2] = [("amd64", "linux-x86_64"), ("arm64", "linux-arm64")];
const ANOTHER: &str = "Package: another\nVersion: 9.9.9\nArchitecture: amd64\nSHA256: ee\nFilename: pool/another/another_9.9.9_amd64.deb\n\n";

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

fn tar_gz(path: &str, content: &[u8]) -> Vec<u8> {
    let gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    let mut tar = tar::Builder::new(gzip);
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, path, content).expect("append");
    tar.into_inner()
        .and_then(flate2::write::GzEncoder::finish)
        .expect("finish")
}

fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, content) in members {
        out.extend_from_slice(
            format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                "100644",
                content.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(content);
        if content.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// A minimal package of `kbf` whose data differs by architecture.
fn deb(arch: &str) -> Vec<u8> {
    let control = format!(
        "Package: kbf\nVersion: {VERSION}\nArchitecture: {arch}\nMaintainer: a <a@example.com>\nInstalled-Size: 1\nDescription: a synopsis\n"
    );
    ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", &tar_gz("./control", control.as_bytes())),
        ("data.tar.gz", &tar_gz("./usr/bin/kbf", arch.as_bytes())),
    ])
}

fn asset(platform: &str) -> String {
    format!("kbf-native-{COMMIT}-{platform}.deb")
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

/// The release's two packages in `dir`.
fn release_files(dir: &Path) {
    for (arch, platform) in ARCHS {
        write(&dir.join(asset(platform)), deb(arch));
    }
}

/// A directory as `apt read` writes it: `Packages` of `suite` per architecture, a `Release`
/// when one is given, and `etags.txt`.
fn index_dir(dir: &Path, suite: &str, packages: &[(&str, &str)], release: Option<&str>) {
    for (arch, body) in packages {
        write(
            &dir.join(format!("dists/{suite}/main/binary-{arch}/Packages")),
            body,
        );
    }
    if let Some(release) = release {
        write(&dir.join(format!("dists/{suite}/Release")), release);
    }
    write(&dir.join("etags.txt"), "Release\t\"abc\"\n");
}

fn run(args: &[&str]) -> Output {
    Command::new(XTASK)
        .args(["channels", "apt"])
        .args(args)
        .output()
        .expect("run xtask")
}

fn path(p: &Path) -> &str {
    p.to_str().expect("path")
}

fn index_args<'a>(
    current: &'a Path,
    out: &'a Path,
    debs: &'a Path,
    commit: &'a str,
) -> Vec<&'a str> {
    vec![
        "index",
        "--suite",
        "testing",
        "--current",
        path(current),
        "--out",
        path(out),
        "--debs",
        path(debs),
        "--commit",
        commit,
        "--version",
        VERSION,
        "--origin",
        "komira",
        "--label",
        "komira",
        "--description",
        "komira packages",
    ]
}

#[test]
fn apt_index_lists_both_packages_with_their_digests_and_keeps_another_products_entry() {
    // Catches: the command line not reaching the merge, a digest other than the file's, and the
    // other product's entry being dropped.
    let dir = scratch("apt-index-ok");
    let (current, out, debs) = (dir.join("current"), dir.join("out"), dir.join("debs"));
    index_dir(
        &current,
        "testing",
        &[("amd64", ANOTHER)],
        Some("Origin: komira\nSuite: testing\n"),
    );
    release_files(&debs);
    let result = run(&index_args(&current, &out, &debs, COMMIT));
    assert_eq!(result.status.code(), Some(0), "{result:?}");
    assert_eq!(
        text(&result.stdout),
        "apt index: added 2, dropped 0, listed 3\n"
    );
    for (arch, platform) in ARCHS {
        let packages =
            std::fs::read_to_string(out.join(format!("dists/testing/main/binary-{arch}/Packages")))
                .expect("Packages");
        let file = format!("kbf_{VERSION}_{arch}.deb");
        assert!(
            packages.contains(&format!("Filename: pool/kbf/{file}\n")),
            "{packages}"
        );
        let digest = sha256(&std::fs::read(debs.join(asset(platform))).expect("deb"));
        assert!(
            packages.contains(&format!("SHA256: {digest}\n")),
            "{packages}"
        );
        assert_eq!(packages.contains("Package: another"), arch == "amd64");
    }
    let release = std::fs::read_to_string(out.join("dists/testing/Release")).expect("Release");
    assert!(release.contains("main/binary-amd64/Packages"), "{release}");
    assert!(release.contains("main/binary-arm64/Packages"), "{release}");
    assert_eq!(
        std::fs::read_to_string(out.join("etags.txt")).expect("etags"),
        "Release\t\"abc\"\n"
    );
}

#[test]
fn apt_index_without_one_package_fails_and_writes_nothing() {
    // Catches: a half-written index when the arm64 package is not among the files.
    let dir = scratch("apt-index-missing");
    let (current, out, debs) = (dir.join("current"), dir.join("out"), dir.join("debs"));
    index_dir(&current, "testing", &[("amd64", ANOTHER)], None);
    write(&debs.join(asset("linux-x86_64")), deb("amd64"));
    let result = run(&index_args(&current, &out, &debs, COMMIT));
    assert_ne!(result.status.code(), Some(0), "{result:?}");
    assert!(
        text(&result.stderr).contains(&asset("linux-arm64")),
        "{result:?}"
    );
    assert!(!out.exists());
}

#[test]
fn apt_index_with_a_commit_that_is_not_a_full_commit_fails_and_writes_nothing() {
    // Catches: a short commit naming files by prefix.
    let dir = scratch("apt-index-commit");
    let (current, out, debs) = (dir.join("current"), dir.join("out"), dir.join("debs"));
    index_dir(&current, "testing", &[("amd64", ANOTHER)], None);
    release_files(&debs);
    let result = run(&index_args(&current, &out, &debs, "0123456"));
    assert_ne!(result.status.code(), Some(0), "{result:?}");
    assert!(
        text(&result.stderr).contains("not a full commit"),
        "{result:?}"
    );
    assert!(!out.exists());
}

/// What a suite lists of the release: both packages, with `digests` per architecture.
fn listing(digests: &[&str; 2]) -> Vec<(&'static str, String)> {
    ARCHS
        .iter()
        .zip(digests)
        .map(|((arch, _), digest)| {
            (
                *arch,
                format!(
                    "Package: kbf\nVersion: {VERSION}\nArchitecture: {arch}\nSHA256: {digest}\nFilename: pool/kbf/kbf_{VERSION}_{arch}.deb\n\n"
                ),
            )
        })
        .collect()
}

struct Promotion {
    testing: PathBuf,
    stable: PathBuf,
    out: PathBuf,
    sums: PathBuf,
}

/// `testing` lists the release with `listed` digests, `stable` lists another product only, and
/// `SHA256SUMS` lists `summed`.
fn promotion(dir: &TestDir, listed: &[&str; 2], summed: &[&str; 2]) -> Promotion {
    let p = Promotion {
        testing: dir.join("testing"),
        stable: dir.join("stable"),
        out: dir.join("out"),
        sums: dir.join("SHA256SUMS"),
    };
    let packages = listing(listed);
    let refs: Vec<(&str, &str)> = packages.iter().map(|(a, t)| (*a, t.as_str())).collect();
    index_dir(&p.testing, "testing", &refs, None);
    index_dir(
        &p.stable,
        "stable",
        &[("amd64", ANOTHER)],
        Some("Origin: komira\nSuite: stable\n"),
    );
    let mut sums = String::new();
    for ((_, platform), digest) in ARCHS.iter().zip(summed) {
        sums.push_str(&format!("{digest}  {}\n", asset(platform)));
    }
    write(&p.sums, sums);
    p
}

fn promote(p: &Promotion, commit: &str) -> Output {
    run(&[
        "promote",
        "--testing",
        path(&p.testing),
        "--stable",
        path(&p.stable),
        "--out",
        path(&p.out),
        "--version",
        VERSION,
        "--sums",
        path(&p.sums),
        "--commit",
        commit,
        "--origin",
        "komira",
        "--label",
        "komira",
        "--description",
        "komira packages",
    ])
}

#[test]
fn apt_promote_lists_the_tested_entries_in_stable_and_keeps_the_others() {
    // Catches: stable not gaining the entries testing lists, or losing another product's.
    let dir = scratch("apt-promote-ok");
    let (a, b) = (sha256(b"amd64 deb"), sha256(b"arm64 deb"));
    let p = promotion(&dir, &[&a, &b], &[&a, &b]);
    let result = promote(&p, COMMIT);
    assert_eq!(result.status.code(), Some(0), "{result:?}");
    assert_eq!(
        text(&result.stdout),
        "apt promote: added 2, dropped 0, listed 3\n"
    );
    let amd64 =
        std::fs::read_to_string(p.out.join("dists/stable/main/binary-amd64/Packages")).expect("p");
    let arm64 =
        std::fs::read_to_string(p.out.join("dists/stable/main/binary-arm64/Packages")).expect("p");
    assert!(amd64.contains(&format!("SHA256: {a}\n")), "{amd64}");
    assert!(
        amd64.contains("Filename: pool/kbf/kbf_1.2.3_amd64.deb\n"),
        "{amd64}"
    );
    assert!(amd64.contains("Package: another"), "{amd64}");
    assert!(arm64.contains(&format!("SHA256: {b}\n")), "{arm64}");
    assert!(p.out.join("dists/stable/Release").is_file());
    assert_eq!(
        std::fs::read_to_string(p.out.join("etags.txt")).expect("etags"),
        "Release\t\"abc\"\n"
    );
}

#[test]
fn apt_promote_refuses_a_digest_that_differs_from_sha256sums_and_names_the_package() {
    // Catches: promoting bytes other than the ones the release was checked against.
    let dir = scratch("apt-promote-digest");
    let (a, b, other) = (sha256(b"amd64 deb"), sha256(b"arm64 deb"), sha256(b"other"));
    let p = promotion(&dir, &[&a, &b], &[&a, &other]);
    let result = promote(&p, COMMIT);
    assert_ne!(result.status.code(), Some(0), "{result:?}");
    let err = text(&result.stderr);
    assert!(err.contains("kbf_1.2.3_arm64.deb"), "{err}");
    assert!(err.contains(&other) && err.contains(&b), "{err}");
    assert!(!p.out.exists());
}
