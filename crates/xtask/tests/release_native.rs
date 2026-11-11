//! `cargo xtask release check` over the native `kbf` deb, `kbf-native-<commit>-<platform>.deb`:
//! a release that lists it and its provenance bundle passes, and one whose bundle is missing is
//! refused naming the file.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kbf_testkit::dir::TestDir;
use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";
const PLATFORMS: [&str; 2] = ["linux-x86_64", "linux-arm64"];

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-native"),
        name,
    )
}

fn sha_of(text: &str) -> String {
    hex::encode(sha2::Sha256::digest(text))
}

/// A provenance bundle of the shape `release check` reads, over the digest `sha256`.
fn bundle_text(sha256: &str) -> String {
    let statement = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "asset", "digest": { "sha256": sha256 } }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {},
    });
    let bundle = serde_json::json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": {
            "payloadType": "application/vnd.in-toto+json",
            "signatures": [],
            "payload": STANDARD.encode(statement.to_string()),
        },
    });
    format!("{bundle}\n")
}

/// A directory with, per platform, a tarball, the `kbf` deb and the native deb of `commit` (the name of the
/// native one made with `native_name`), `SHA256SUMS` over them, and a bundle for each.
fn make_dist(root: &Path, commit: &str, native_name: impl Fn(&str, &str) -> String) -> PathBuf {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).expect("dist");
    let mut sums = String::new();
    for platform in PLATFORMS {
        for name in [
            format!("kbf-daemon-{commit}-{platform}.tar.gz"),
            format!("kbf-{commit}-{platform}.deb"),
            native_name(commit, platform),
        ] {
            let bytes = format!("{name} bytes");
            std::fs::write(dist.join(&name), &bytes).expect("deb");
            let sha = sha_of(&bytes);
            sums.push_str(&format!("{sha}  {name}\n"));
            std::fs::write(
                dist.join(format!("{name}.sigstore.json")),
                bundle_text(&sha),
            )
            .expect("bundle");
        }
    }
    std::fs::write(dist.join("SHA256SUMS"), sums).expect("SHA256SUMS");
    dist
}

fn native(commit: &str, platform: &str) -> String {
    format!("kbf-native-{commit}-{platform}.deb")
}

fn check(dir: &Path, commit: &str) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .args(["release", "check"])
        .arg(dir)
        .args(["--commit", commit])
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// Asserts that a release listing the native deb of each Linux platform beside the `kbf` deb, each
/// with its provenance bundle, is accepted as assets of the commit and the bundles are counted.
#[test]
fn check_accepts_the_native_deb_of_the_commit_with_its_bundle() {
    let root = scratch("check-native");
    let dist = make_dist(&root, COMMIT, native);
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "sums: 6 files match; the flipped copy does not\n\
             names: every listed file is an asset of {COMMIT}\n\
             bundles: each of the 2 tarballs and 4 package(s) has one provenance bundle\n"
        )
    );
}

/// Asserts that a native deb whose bundle is missing is refused naming the deb and the bundle
/// expected, and that a native deb named for another commit is not an asset of this one.
#[test]
fn check_refuses_a_native_deb_with_no_bundle_or_of_another_commit() {
    let root = scratch("check-native-no-bundle");
    let dist = make_dist(&root, COMMIT, native);
    let name = native(COMMIT, "linux-arm64");
    std::fs::remove_file(dist.join(format!("{name}.sigstore.json"))).expect("remove");
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {name}: no provenance bundle {name}.sigstore.json\n")
    );
    let root = scratch("check-native-other");
    let other = make_dist(&root, COMMIT, |_, platform| native(OTHER, platform));
    let (code, _, err) = check(&other, COMMIT);
    assert_eq!(code, Some(1));
    assert!(
        err.contains(&format!(
            "{}: not an asset of {COMMIT}",
            native(OTHER, "linux-x86_64")
        )),
        "{err}"
    );
}
