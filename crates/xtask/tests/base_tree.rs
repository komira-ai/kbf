//! `cargo xtask base-tree` run as the artifacts workflow runs it: the `xtask` binary, its flags
//! parsed, the verb dispatched. A built tree needs the pinned base's own layers, which are not in
//! the checkout, so these runs end at the checks that come before any tree is built: the
//! platform, the image against the pinned base, the flags. Each such run writes nothing.
//! The tree itself is tested in `src/base_tree/tests.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;
use sha2::Digest;
use xtask::package::Platform;
use xtask::pkg_image_asset::Image;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";

/// `sha256:<hex>` of `bytes`.
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}

/// A plain tar of `files` (path, bytes), each a regular file.
fn tar_of(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(bytes.len() as u64);
        tar.append_data(&mut h, path, bytes.as_slice())
            .expect("append");
    }
    tar.into_inner().expect("tar")
}

/// An OCI image layout tar of one manifest whose one layer holds `/etc/os`: an image that is not
/// the pinned base's. Gives the tar and the digest of its layer.
fn image() -> (Vec<u8>, String) {
    let layer = tar_of(&[("etc/os".to_owned(), b"not the base".to_vec())]);
    let config = br#"{"os":"linux","architecture":"amd64"}"#.to_vec();
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "config": { "digest": digest(&config) },
        "layers": [{ "digest": digest(&layer) }],
    })
    .to_string()
    .into_bytes();
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{ "digest": digest(&manifest) }],
    })
    .to_string()
    .into_bytes();
    let blob = |b: &[u8]| format!("blobs/sha256/{}", hex::encode(sha2::Sha256::digest(b)));
    let layer_digest = digest(&layer);
    let files = vec![
        ("index.json".to_owned(), index),
        (blob(&manifest), manifest),
        (blob(&config), config),
        (blob(&layer), layer),
    ];
    (tar_of(&files), layer_digest)
}

/// A scratch directory holding `image.oci.tar` and `kbf-task`; the verb's `--out` is `dist`,
/// which no test makes.
struct Fx {
    root: TestDir,
    layer: String,
}

impl Fx {
    fn new(name: &str) -> Self {
        let root = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-base-tree"),
            name,
        );
        let (bytes, layer) = image();
        std::fs::write(root.path().join("image.oci.tar"), bytes).expect("image");
        std::fs::write(root.path().join("kbf-task"), b"kbf-task").expect("binary");
        Self { root, layer }
    }

    fn out(&self) -> PathBuf {
        self.root.path().join("dist")
    }

    /// `xtask base-tree` with the workflow's flags for `platform`, and `extra` after them.
    fn run(&self, platform: &str, extra: &[&str]) -> Output {
        Command::new(XTASK)
            .arg("base-tree")
            .arg("--image")
            .arg(self.root.path().join("image.oci.tar"))
            .args(["--platform", platform, "--commit", COMMIT])
            .arg("--binary")
            .arg(self.root.path().join("kbf-task"))
            .arg("--out")
            .arg(self.out())
            .args(extra)
            .output()
            .expect("run xtask")
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Asserts the verb, run as the binary with the workflow's flags, checks the image against the
/// pinned base of each Linux platform: it exits 1, says which layer is not the base's (both
/// digests named) on stderr, prints nothing on stdout, and makes no `--out`. Catches: a verb
/// that is not wired to its name, a flag the binary does not parse as the workflow passes it,
/// a mismatch that exits 0 or writes a bundle, and a refusal that does not say what differs.
#[test]
fn the_binary_refuses_an_image_that_is_not_the_pinned_base_and_writes_nothing() {
    for (name, platform) in [
        ("linux-x86_64", Platform::LinuxX86_64),
        ("linux-arm64", Platform::LinuxArm64),
    ] {
        let fx = Fx::new(name);
        let pinned = Image::Task
            .base(platform)
            .expect("the pinned base")
            .expect("a Linux platform has one");
        let out = fx.run(name, &[]);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert_eq!(
            stderr(&out),
            format!(
                "xtask: layer 1 is {}, the base's is {}\n",
                fx.layer, pinned.layers[0]
            )
        );
        assert!(out.stdout.is_empty());
        assert!(!fx.out().exists());
    }
}

/// Asserts the binary refuses the macOS platform by name, exit 1, and writes nothing. Catches: a
/// release job of the Mac that would build a base tree.
#[test]
fn the_binary_refuses_the_macos_platform() {
    let fx = Fx::new("darwin");
    let out = fx.run("darwin-arm64", &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "xtask: darwin-arm64: the kbf base tree is built for the Linux platforms only\n"
    );
    assert!(!fx.out().exists());
}

/// Asserts a flag the verb does not know, a platform that is none, and a missing flag are usage
/// errors, exit 2, before the image is read. Catches: a verb that takes a misspelt flag of the
/// workflow silently, or runs without one it needs.
#[test]
fn a_bad_flag_is_a_usage_error() {
    let fx = Fx::new("usage");
    for (platform, extra, says) in [
        (
            "linux-x86_64",
            &["--force"][..],
            "unexpected argument '--force'",
        ),
        ("linux-riscv", &[][..], "invalid value 'linux-riscv'"),
    ] {
        let out = fx.run(platform, extra);
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
        assert!(stderr(&out).contains(says), "{}", stderr(&out));
    }
    let out = Command::new(XTASK)
        .args([
            "base-tree",
            "--platform",
            "linux-x86_64",
            "--commit",
            COMMIT,
        ])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("the following required arguments were not provided"),
        "{}",
        stderr(&out)
    );
    assert!(!fx.out().exists());
}
