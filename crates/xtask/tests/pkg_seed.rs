//! `cargo xtask pkg-seed` run as CI runs it, over a fake release in a scratch directory:
//! what is tested here is the command line, the exit status and what lands on disk; the
//! checks themselves are tested in `src/pkg_seed/tests.rs`. The tarball holds a stand-in
//! for `kbf` and the bundle is a JSON object that says it is a fixture: nothing here is
//! a real release or a real attestation.

use kbf_installer::seed::Manifest;
use kbf_testkit::dir::TestDir;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";
const MISSING: &str = "is missing or is not a regular file";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pkg-seed"),
        name,
    )
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn put(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).expect("write");
}

fn tgz(files: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let gzip = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gzip);
    for (path, mode, bytes) in files {
        let mut h = tar::Header::new_ustar();
        h.set_path(format!("./{path}")).expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(*mode);
        h.set_cksum();
        tar.append(&h, *bytes).expect("append");
    }
    tar.into_inner().expect("tar").finish().expect("gzip")
}

/// A fake release of `COMMIT` for linux-x86_64 in `dir/dist`, and three trust files in
/// `dir/trust`.
fn release(dir: &Path) {
    let (dist, trust) = (dir.join("dist"), dir.join("trust"));
    std::fs::create_dir_all(&dist).expect("dist");
    std::fs::create_dir_all(&trust).expect("trust");
    let stem = format!("kbf-{COMMIT}-linux-x86_64");
    let file = |ext: &str| dist.join(format!("{stem}{ext}"));
    let tarball = tgz(&[("kbf", 0o755, b"a stand-in for kbf")]);
    put(&file(".tar.gz"), tarball);
    put(
        &file(".tar.gz.sigstore.json"),
        "{\"fake\":\"a test fixture, not a sigstore bundle\"}",
    );
    let short = &COMMIT[..12];
    let facts = serde_json::json!({
        "commit": short,
        "min_protocol": 3,
        "platform": "linux-x86_64",
        "profile": "release",
        "protocol": 4,
        "seq": 801,
        "state_schema": 7,
        "version": "0.1.0",
    });
    put(&file(".version.json"), facts.to_string());
    for name in ["gh", "installer.conf", "trusted_root.jsonl"] {
        put(&trust.join(name), name);
    }
}

/// `pkg-seed` over `dir`'s release and trust files, for `commit` and `platform`, into `out`.
fn pkg_seed(dir: &Path, commit: &str, platform: &str, out: &Path) -> Output {
    Command::new(XTASK)
        .args(["pkg-seed", "--dist"])
        .arg(dir.join("dist"))
        .args(["--commit", commit, "--platform", platform, "--trust"])
        .arg(dir.join("trust"))
        .arg("--out")
        .arg(out)
        .output()
        .expect("run xtask")
}

/// The names in `dir`, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn pkg_seed_writes_the_four_files_and_does_not_write_over_them() {
    // Catches: the verb not wired to its writer (no directory, or one that is not the four
    // files), a manifest the installer's own reader refuses, a file written executable, and
    // a second run that writes into the directory the first made.
    let dir = scratch("happy");
    release(&dir);
    let out = dir.join("seed");
    let done = pkg_seed(&dir, COMMIT, "linux-x86_64", &out);
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
    let stem = format!("kbf-{COMMIT}-linux-x86_64");
    let want = [
        format!("{stem}.tar.gz"),
        format!("{stem}.tar.gz.sigstore.json"),
        format!("{stem}.version.json"),
        "manifest.json".to_owned(),
    ];
    assert_eq!(listing(&out), want);
    let manifest = std::fs::read(out.join("manifest.json")).expect("manifest");
    let parsed = Manifest::parse(&manifest).expect("parses");
    assert_eq!(parsed.commit.as_str(), COMMIT);
    assert_eq!(parsed.platform, "linux-x86_64");
    assert_eq!(parsed.seq, 801);
    for name in &want {
        let meta = std::fs::metadata(out.join(name)).expect("stat");
        let mode = meta.permissions().mode();
        assert_eq!(mode & 0o111, 0, "{name} is executable");
    }
    let said = text(&done.stdout);
    assert!(said.contains("manifest.json"), "{said}");

    let again = pkg_seed(&dir, COMMIT, "linux-x86_64", &out);
    assert_eq!(again.status.code(), Some(1), "{}", text(&again.stderr));
    let said = text(&again.stderr);
    assert!(said.contains("already exists"), "{said}");
}

#[test]
fn pkg_seed_refuses_another_commits_release_and_writes_nothing() {
    // Catches: files found by what the directory holds instead of by the commit named (the
    // release of one commit made into the seed of another), a refusal that leaves a
    // half-made directory, and a refusal that does not say what is missing.
    let dir = scratch("other-commit");
    release(&dir);
    let out = dir.join("seed");
    let refused = pkg_seed(&dir, OTHER, "linux-x86_64", &out);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
    let said = text(&refused.stderr);
    let name = format!("kbf-{OTHER}-linux-x86_64.tar.gz");
    assert!(said.starts_with("xtask: "), "{said}");
    assert!(said.contains(&name), "{said}");
    assert!(said.contains(MISSING), "{said}");
    assert!(!out.exists());

    let arm = pkg_seed(&dir, COMMIT, "linux-arm64", &out);
    assert_eq!(arm.status.code(), Some(1), "{}", text(&arm.stderr));
    let said = text(&arm.stderr);
    assert!(said.contains("linux-arm64.tar.gz"), "{said}");
    assert!(!out.exists());
}

#[test]
fn pkg_seed_says_usage_for_a_short_commit_or_a_mac_and_reads_the_trust_directory() {
    // Catches: a short commit making a seed of short names, a Mac release made into a Linux
    // seed, a platform nobody builds accepted, and a seed made when the trust directory
    // cannot be read.
    let dir = scratch("usage");
    release(&dir);
    let out = dir.join("seed");
    let short = pkg_seed(&dir, "0123456789ab", "linux-x86_64", &out);
    assert_eq!(short.status.code(), Some(2), "{}", text(&short.stderr));
    assert!(text(&short.stderr).contains("not a full commit SHA"));
    let mac = pkg_seed(&dir, COMMIT, "darwin-arm64", &out);
    assert_eq!(mac.status.code(), Some(2), "{}", text(&mac.stderr));
    assert!(text(&mac.stderr).contains("only Linux seeds are made"));
    let nobody = pkg_seed(&dir, COMMIT, "linux-riscv", &out);
    assert_eq!(nobody.status.code(), Some(2), "{}", text(&nobody.stderr));
    assert!(!out.exists());

    std::fs::remove_file(dir.join("trust/gh")).expect("remove");
    let lost = pkg_seed(&dir, COMMIT, "linux-x86_64", &out);
    assert_eq!(lost.status.code(), Some(1), "{}", text(&lost.stderr));
    let said = text(&lost.stderr);
    assert!(said.contains("trust/gh"), "{said}");
    assert!(!out.exists());
}
