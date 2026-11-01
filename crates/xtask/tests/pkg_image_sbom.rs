//! `cargo xtask pkg-image-sbom` run as the build and verify jobs run it, with `xtask-fake` as
//! the `syft` it runs (it records its working directory, arguments and environment, and prints the
//! SBOM a test wrote for it). The logic is tested in `src/pkg_image_sbom/tests.rs`; what is tested
//! here is the command line: the flags, what syft is run with, and what the files the job uploads
//! hold.

use kbf_testkit::dir::TestDir;
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::process::Command;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const IMAGE: &str = "kbf-task-image-0123456789abcdef0123456789abcdef01234567-linux-x86_64";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pkg-image-sbom"),
        name,
    )
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The tar of an image layout (an index, a manifest and a config) and the digest of its manifest.
fn image_layout() -> (Vec<u8>, String) {
    let config = serde_json::json!({"os": "linux", "architecture": "amd64"})
        .to_string()
        .into_bytes();
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "config": {"digest": format!("sha256:{}", sha(&config))},
        "layers": [],
    })
    .to_string()
    .into_bytes();
    let digest = format!("sha256:{}", sha(&manifest));
    let index = serde_json::json!({"schemaVersion": 2, "manifests": [{"digest": digest}]})
        .to_string()
        .into_bytes();
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in [
        ("index.json".to_owned(), index),
        (format!("blobs/sha256/{}", sha(&manifest)), manifest),
        (format!("blobs/sha256/{}", sha(&config)), config),
    ] {
        let mut h = tar::Header::new_ustar();
        h.set_path(&path).expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append(&h, bytes.as_slice()).expect("append");
    }
    (tar.into_inner().expect("tar"), digest)
}

/// What syft prints of the image with the manifest `manifest`, stamped with `serial` and `time`.
fn syft_says(manifest: &str, serial: &str, time: &str) -> String {
    serde_json::json!({
        "bomFormat": "CycloneDX",
        "serialNumber": serial,
        "metadata": {
            "timestamp": time,
            "component": {"type": "container", "name": "image", "version": manifest},
        },
        "components": [
            {"type": "library", "name": "libc6", "purl": "pkg:deb/ubuntu/libc6@2.39?arch=amd64"},
        ],
    })
    .to_string()
}

/// A world of one job: `<root>/dist` holds the image asset, `<root>/tools/syft` is the fake
/// printing `says`. Returns (root, dist, syft, manifest digest).
fn world(name: &str, says: impl Fn(&str) -> String) -> (TestDir, PathBuf, PathBuf, String) {
    let root = scratch(name);
    let dist = root.join("dist");
    let tools = root.join("tools");
    std::fs::create_dir_all(&dist).expect("dist");
    std::fs::create_dir_all(&tools).expect("tools");
    let (tar, manifest) = image_layout();
    std::fs::write(dist.join(format!("{IMAGE}.oci.tar")), tar).expect("image");
    let syft = tools.join("syft");
    std::fs::hard_link(FAKE, &syft).expect("link the fake as syft");
    std::fs::write(tools.join("syft.json"), says(&manifest)).expect("syft.json");
    (root, dist, syft, manifest)
}

/// `cargo xtask pkg-image-sbom ...` with `args`; the exit code, stdout and stderr.
fn xtask(args: &[String]) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .arg("pkg-image-sbom")
        .args(args)
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// `words` as owned arguments.
fn owned(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The arguments that make the SBOM of the image in `dist`.
fn make_args(dist: &Path, syft: &Path) -> Vec<String> {
    let image = dist.join(format!("{IMAGE}.oci.tar"));
    owned(&[
        "--service",
        "task",
        "--image",
        s(&image),
        "--platform",
        "linux-x86_64",
        "--commit",
        COMMIT,
        "--syft",
        s(syft),
        "--source-date-epoch",
        "1788220800",
    ])
}

/// The arguments that verify the SBOM beside the image in `dist`.
fn verify_args(dist: &Path) -> Vec<String> {
    let image = dist.join(format!("{IMAGE}.oci.tar"));
    owned(&[
        "--verify-only",
        "--service",
        "task",
        "--image",
        s(&image),
        "--platform",
        "linux-x86_64",
        "--commit",
        COMMIT,
    ])
}

#[test]
fn the_sbom_syft_prints_is_written_beside_the_image_listed_and_verified() {
    // Catches: the SBOM missing from `dist` or from the platform's sums, listed with another digest
    // than its bytes or printed as another line than the one listed, left with the serial number
    // and time syft stamped, not the image's, or an SBOM that the verify step then refuses. (What
    // syft is run with, directory, arguments and environment, is the unit tests'.)
    let (_root, dist, syft, manifest) = world("make", |m| syft_says(m, "urn:uuid:random", "later"));
    let (code, out, err) = xtask(&make_args(&dist, &syft));
    assert_eq!(code, Some(0), "{err}");
    let name = format!("{IMAGE}.cdx.json");
    let written = std::fs::read(dist.join(&name)).expect("the SBOM is beside the image");
    assert_eq!(
        out,
        format!("made {name}:\n{}  {name}\n", sha(&written)),
        "the line printed is the line listed"
    );
    assert_eq!(
        std::fs::read_to_string(dist.join("SHA256SUMS.linux-x86_64")).expect("sums"),
        format!("{}  {name}\n", sha(&written))
    );
    let bom: serde_json::Value = serde_json::from_slice(&written).expect("JSON");
    assert_ne!(bom["serialNumber"], "urn:uuid:random");
    assert!(
        bom["serialNumber"]
            .as_str()
            .is_some_and(|n| n.starts_with("urn:uuid:")),
        "{bom}"
    );
    assert_eq!(bom["metadata"]["timestamp"], "2026-09-01T00:00:00Z");
    assert_eq!(bom["metadata"]["component"]["version"], manifest);
    let (code, out, err) = xtask(&verify_args(&dist));
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, format!("{name}: the SBOM of the image\n"));
}

#[test]
fn the_same_image_has_the_same_sbom_bytes_from_two_runs_of_syft() {
    // Catches: an SBOM that is not reproducible: two jobs (or two runs of one) on the same image
    // whose syft stamped different serial numbers and times must leave the same file.
    let first = world("repro-1", |m| syft_says(m, "urn:uuid:one", "one"));
    let second = world("repro-2", |m| syft_says(m, "urn:uuid:two", "two"));
    for (_, dist, syft, _) in [&first, &second] {
        let (code, _, err) = xtask(&make_args(dist, syft));
        assert_eq!(code, Some(0), "{err}");
    }
    let name = format!("{IMAGE}.cdx.json");
    assert_eq!(
        std::fs::read(first.1.join(&name)).expect("first"),
        std::fs::read(second.1.join(&name)).expect("second")
    );
}

#[test]
fn a_failing_syft_or_a_wrong_sbom_leaves_no_file_and_no_line() {
    // Catches: an SBOM or a sums line left behind by a run that failed (here syft is `false`),
    // syft's failure lost from the message, and an SBOM of another image written because syft ran.
    let (_root, dist, _, _) = world("failing", |m| syft_says(m, "urn:uuid:one", "one"));
    let (code, out, err) = xtask(&make_args(&dist, Path::new("/usr/bin/false")));
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.starts_with("xtask: syft scan oci-archive:") && err.contains("--output cyclonedx-json"),
        "{err}"
    );
    let name = format!("{IMAGE}.cdx.json");
    assert!(!dist.join(&name).exists());
    assert!(!dist.join("SHA256SUMS.linux-x86_64").exists());

    let (_root, dist, syft, _) = world("wrong", |_| {
        syft_says("sha256:other", "urn:uuid:one", "one")
    });
    let (code, out, err) = xtask(&make_args(&dist, &syft));
    assert_eq!(code, Some(1), "{out}");
    assert!(err.contains("the SBOM describes sha256:other"), "{err}");
    assert!(!dist.join(&name).exists());
    assert!(!dist.join("SHA256SUMS.linux-x86_64").exists());
}

#[test]
fn the_verify_step_refuses_a_missing_or_another_images_sbom() {
    // Catches: a verify job that passes an image with no SBOM beside it, or one with another
    // image's, which the attest job would sign as this image's.
    let (_root, dist, _, _) = world("verify", |m| syft_says(m, "urn:uuid:one", "one"));
    let (code, _, err) = xtask(&verify_args(&dist));
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("the image has no SBOM"), "{err}");
    std::fs::write(
        dist.join(format!("{IMAGE}.cdx.json")),
        syft_says("sha256:other", "urn:uuid:one", "one"),
    )
    .expect("sbom");
    let (code, _, err) = xtask(&verify_args(&dist));
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("not the SBOM of the image with the manifest"),
        "{err}"
    );
}

#[test]
fn the_flags_fit_together_and_only_the_task_image_has_an_sbom() {
    // Catches: `--verify-only` that also runs syft (`--syft` is refused with it), a build step with
    // no syft or no time (it would make an SBOM that is not reproducible), and an SBOM asked of the
    // frontend or coordinator image, which the release has none of.
    let (_root, dist, syft, _) = world("flags", |m| syft_says(m, "urn:uuid:one", "one"));
    let mut with_syft = verify_args(&dist);
    with_syft.extend(owned(&["--syft", s(&syft)]));
    let (code, _, err) = xtask(&with_syft);
    assert_eq!(code, Some(2), "{err}");
    for dropped in ["--syft", "--source-date-epoch"] {
        let args = make_args(&dist, &syft);
        let at = args.iter().position(|a| a == dropped).expect("flag");
        let mut args = args;
        args.drain(at..=at + 1);
        let (code, _, err) = xtask(&args);
        assert_eq!(code, Some(2), "{dropped}: {err}");
        assert!(err.contains(dropped), "{dropped}: {err}");
    }
    for service in ["frontend", "coordinator"] {
        let mut args = make_args(&dist, &syft);
        let at = args.iter().position(|a| a == "task").expect("service");
        args[at] = service.to_owned();
        let (code, _, err) = xtask(&args);
        assert_eq!(code, Some(2), "{service}: {err}");
        assert!(
            err.contains("has no SBOM asset: only the task image does"),
            "{service}: {err}"
        );
    }
}
