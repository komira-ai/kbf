use std::path::PathBuf;

use super::*;
use crate::fetch::Sha256;
use crate::package::tarball as binary_tarball;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-pkg-image-asset-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A plain tar of `files` (path, mode, bytes).
fn tar_of(files: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, mode, bytes) in files {
        let mut h = tar::Header::new_ustar();
        h.set_path(path).expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(*mode);
        h.set_cksum();
        tar.append(&h, *bytes).expect("append");
    }
    tar.into_inner().expect("tar")
}

/// `bytes` gzip-compressed.
fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut gz = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).expect("gzip");
    gz.finish().expect("gzip")
}

/// The blob path of `bytes` in a layout.
fn blob_path(bytes: &[u8]) -> String {
    format!("blobs/sha256/{}", Sha256::of(bytes))
}

/// What a good frontend image holds, each of which a test may change.
struct Fix {
    kbf: Vec<u8>,
    kbf_mode: u32,
    arch: &'static str,
    os: &'static str,
    user: &'static str,
    entrypoint: Vec<&'static str>,
    /// Gzip the layers (buildx does) rather than leave them plain.
    gzip: bool,
    /// Layers after the one that holds `kbf`, as (path, mode, bytes) of one file each.
    later: Vec<(&'static str, u32, Vec<u8>)>,
    /// Spoil the layer blob's name so it no longer names its bytes.
    spoil_layer: bool,
    /// Leave out `index.json`.
    no_index: bool,
}

impl Fix {
    fn new() -> Fix {
        Fix {
            kbf: b"\x7fELF the kbf built for the commit".to_vec(),
            kbf_mode: 0o755,
            arch: "amd64",
            os: "linux",
            user: "65532:65532",
            entrypoint: vec!["/usr/bin/kbf", "--mode=frontend"],
            gzip: true,
            later: Vec::new(),
            spoil_layer: false,
            no_index: false,
        }
    }

    /// The image: an OCI layout in a tar, as buildx writes one.
    fn image(&self) -> Vec<u8> {
        let mut layers = vec![tar_of(&[
            ("etc/ssl/certs/ca-certificates.crt", 0o644, b"roots"),
            ("usr/bin/kbf", self.kbf_mode, &self.kbf),
        ])];
        for (path, mode, bytes) in &self.later {
            layers.push(tar_of(&[(path, *mode, bytes)]));
        }
        let layers: Vec<Vec<u8>> = layers
            .into_iter()
            .map(|l| if self.gzip { gzip(&l) } else { l })
            .collect();
        let config = serde_json::json!({
            "os": self.os,
            "architecture": self.arch,
            "config": { "User": self.user, "Entrypoint": self.entrypoint },
            "rootfs": { "type": "layers", "diff_ids": [] },
        })
        .to_string()
        .into_bytes();
        let listed: Vec<Value> = layers
            .iter()
            .map(|l| serde_json::json!({ "digest": format!("sha256:{}", Sha256::of(l)) }))
            .collect();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": { "digest": format!("sha256:{}", Sha256::of(&config)) },
            "layers": listed,
        })
        .to_string()
        .into_bytes();
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{ "digest": format!("sha256:{}", Sha256::of(&manifest)) }],
        })
        .to_string()
        .into_bytes();
        let mut named: Vec<(String, Vec<u8>)> = vec![
            (
                "oci-layout".to_owned(),
                br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
            ),
            (blob_path(&manifest), manifest),
            (blob_path(&config), config),
        ];
        if !self.no_index {
            named.push(("index.json".to_owned(), index));
        }
        for (i, layer) in layers.into_iter().enumerate() {
            let path = blob_path(&layer);
            // The first layer's name is spoiled: its bytes no longer hash to it.
            if self.spoil_layer && i == 0 {
                let mut changed = layer;
                changed.push(0);
                named.push((path, changed));
            } else {
                named.push((path, layer));
            }
        }
        let files: Vec<(&str, u32, &[u8])> = named
            .iter()
            .map(|(p, b)| (p.as_str(), 0o644, b.as_slice()))
            .collect();
        tar_of(&files)
    }
}

/// A directory with the attested tarball of `kbf` for `platform` and the image of `fix`:
/// (dir, image path, tarball path).
fn world(name: &str, fix: &Fix, kbf: &[u8], platform: Platform) -> (PathBuf, PathBuf, PathBuf) {
    let dir = scratch(name);
    let tarball = dir.join(format!("kbf-{COMMIT}-{}.tar.gz", platform.name()));
    std::fs::write(&tarball, binary_tarball("kbf", kbf).expect("tarball")).expect("tarball");
    let image = dir.join("image.oci.tar");
    std::fs::write(&image, fix.image()).expect("image");
    (dir, image, tarball)
}

fn check(platform: Platform, tarball: &Path) -> Check<'_> {
    Check {
        platform,
        kbf_tarball: tarball,
    }
}

/// The problems of `fix` as the frontend image for `platform`.
fn found(name: &str, fix: &Fix, platform: Platform) -> Vec<String> {
    let (_, image, tarball) = world(name, fix, &Fix::new().kbf, platform);
    problems(
        &std::fs::read(&image).expect("image"),
        &check(platform, &tarball),
    )
    .expect("problems")
}

/// Asserts that an image of the attested `kbf`, for the platform, running `kbf --mode=frontend`
/// as a numeric non-root user, is accepted as the asset whether its layers are gzip or plain:
/// copied under a name of the commit and platform, with its digest in the sums file.
#[test]
fn an_image_of_the_attested_kbf_is_accepted_as_the_asset() {
    for (platform, arch, gz) in [
        (Platform::LinuxX86_64, "amd64", true),
        (Platform::LinuxArm64, "arm64", false),
    ] {
        let mut fix = Fix::new();
        fix.arch = arch;
        fix.gzip = gz;
        let (dir, image, tarball) = world("good", &fix, &fix.kbf, platform);
        let out = dir.join("dist");
        let c = check(platform, &tarball);
        let printed = asset(&image, COMMIT, &Mode::Add(&out), &c).expect("asset");
        let name = format!("kbf-frontend-image-{COMMIT}-{}.oci.tar", platform.name());
        let bytes = std::fs::read(&image).expect("image");
        assert_eq!(std::fs::read(out.join(&name)).expect("asset"), bytes);
        let line = format!("{}  {name}\n", Sha256::of(&bytes));
        let sums = out.join(format!("SHA256SUMS.{}", platform.name()));
        assert_eq!(std::fs::read_to_string(sums).expect("sums"), line);
        assert_eq!(
            printed,
            vec![format!("packaged {name}:"), line.trim_end().to_owned()]
        );
    }
}

/// Asserts that an image whose `kbf` differs from the attested tarball's by one byte is
/// refused with both digests, that a mode other than 0755 is named, that a later layer which
/// replaces `kbf` is the one judged, and that an image with no `kbf` is named.
#[test]
fn an_image_whose_kbf_is_not_the_attested_one_is_refused() {
    let good = Fix::new().kbf;
    let mut fix = Fix::new();
    *fix.kbf.last_mut().expect("byte") ^= 1;
    let got = found("kbf-differs", &fix, Platform::LinuxX86_64);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("usr/bin/kbf (sha256:"), "{got:?}");
    assert!(
        got[0].contains("is not the kbf of the attested tarball"),
        "{got:?}"
    );
    assert!(
        got[0].contains(&Sha256::of(&fix.kbf).to_string()),
        "{got:?}"
    );
    assert!(got[0].contains(&Sha256::of(&good).to_string()), "{got:?}");

    let mut fix = Fix::new();
    fix.kbf_mode = 0o644;
    assert_eq!(
        found("kbf-mode", &fix, Platform::LinuxX86_64),
        vec!["usr/bin/kbf has mode 0644, not 0755".to_owned()]
    );

    let mut fix = Fix::new();
    fix.later = vec![("usr/bin/kbf", 0o755, b"replaced by a later layer".to_vec())];
    let got = found("kbf-later-layer", &fix, Platform::LinuxX86_64);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].contains("is not the kbf of the attested tarball"),
        "{got:?}"
    );

    let mut fix = Fix::new();
    fix.later = vec![("usr/bin/other", 0o755, b"x".to_vec())];
    assert!(found("kbf-other-layer", &fix, Platform::LinuxX86_64).is_empty());
}

/// Asserts that an image with another architecture or OS than the platform's, a root or unset
/// user, another entrypoint, or no `kbf` at all is refused, each with its own line.
#[test]
fn an_image_for_another_platform_user_or_entrypoint_is_refused() {
    let mut fix = Fix::new();
    fix.arch = "arm64";
    assert_eq!(
        found("arch", &fix, Platform::LinuxX86_64),
        vec!["config says linux/arm64, not linux/amd64".to_owned()]
    );
    let mut fix = Fix::new();
    fix.os = "windows";
    assert_eq!(
        found("os", &fix, Platform::LinuxX86_64),
        vec!["config says windows/amd64, not linux/amd64".to_owned()]
    );
    for user in ["", "0", "root", "0:0", "root:root"] {
        let mut fix = Fix::new();
        fix.user = user;
        assert_eq!(
            found("user", &fix, Platform::LinuxX86_64),
            vec![format!(
                "config runs as user {user:?}, which is root or unset"
            )],
            "{user:?}"
        );
    }
    let mut fix = Fix::new();
    fix.entrypoint = vec!["/usr/bin/kbf", "--mode=daemon"];
    let got = found("entrypoint", &fix, Platform::LinuxX86_64);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("entrypoint is [\"/usr/bin/kbf\", \"--mode=daemon\"]"),
        "{got:?}"
    );
}

/// Asserts that bytes that are not an image, a layout with no `index.json`, a layer whose
/// bytes do not hash to the digest that names it, and a tarball with no `kbf` are each refused.
#[test]
fn a_file_that_is_not_the_image_of_the_attested_tarball_is_refused() {
    let (_, _, tarball) = world("shape", &Fix::new(), &Fix::new().kbf, Platform::LinuxX86_64);
    let c = check(Platform::LinuxX86_64, &tarball);
    let got = problems(b"not a tar at all", &c).expect("problems");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("is not an OCI image tar") || got[0].starts_with("holds no index.json"),
        "{got:?}"
    );

    let mut fix = Fix::new();
    fix.no_index = true;
    assert_eq!(
        found("no-index", &fix, Platform::LinuxX86_64),
        vec!["holds no index.json: not an OCI image layout".to_owned()]
    );

    let mut fix = Fix::new();
    fix.spoil_layer = true;
    let got = found("spoiled", &fix, Platform::LinuxX86_64);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("has another digest"), "{got:?}");

    let dir = scratch("no-kbf-in-tarball");
    let empty = dir.join("kbf.tar.gz");
    std::fs::write(&empty, binary_tarball("other", b"x").expect("tarball")).expect("tarball");
    let got =
        problems(&Fix::new().image(), &check(Platform::LinuxX86_64, &empty)).expect("problems");
    assert_eq!(got, vec![format!("{} holds no kbf", empty.display())]);
}

/// Asserts that `--verify-only` accepts the file named as the asset and refuses the same
/// bytes under another name (another commit or platform), writing nothing either way, and that
/// an asset already listed in the sums is not added twice.
#[test]
fn verify_only_wants_the_name_of_the_commit_and_platform_and_a_listed_image_is_not_added_twice() {
    let fix = Fix::new();
    let (dir, image, tarball) = world("names", &fix, &fix.kbf, Platform::LinuxX86_64);
    let c = check(Platform::LinuxX86_64, &tarball);
    let name = file_name(COMMIT, Platform::LinuxX86_64);
    let named = dir.join(&name);
    std::fs::copy(&image, &named).expect("copy");
    assert_eq!(
        asset(&named, COMMIT, &Mode::VerifyOnly, &c).expect("verify"),
        vec![format!(
            "{name}: a linux-x86_64 frontend image of the kbf in {}",
            tarball.display()
        )]
    );
    for refused in [
        file_name(
            "fedcba9876543210fedcba9876543210fedcba98",
            Platform::LinuxX86_64,
        ),
        file_name(COMMIT, Platform::LinuxArm64),
        "other.oci.tar".to_owned(),
    ] {
        let other = dir.join(&refused);
        std::fs::copy(&image, &other).expect("copy");
        let err = asset(&other, COMMIT, &Mode::VerifyOnly, &c).expect_err(&refused);
        assert!(
            err.to_string().contains(&format!("is not named {name}")),
            "{refused}: {err}"
        );
    }
    let out = dir.join("dist");
    asset(&image, COMMIT, &Mode::Add(&out), &c).expect("first");
    let err = asset(&image, COMMIT, &Mode::Add(&out), &c).expect_err("twice");
    assert!(err.to_string().contains("already lists"), "{err}");
}

/// An image layout with the given `index.json` text and blobs (path, bytes), plus the named
/// extra tar entries, in a tar.
fn raw_image(index: &str, blobs: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut files: Vec<(&str, u32, &[u8])> = vec![("index.json", 0o644, index.as_bytes())];
    for (p, b) in blobs {
        files.push((p.as_str(), 0o644, b.as_slice()));
    }
    tar_of(&files)
}

/// The problems of `image` for x86_64 against the standard attested tarball.
fn problems_of(name: &str, image: &[u8]) -> Vec<String> {
    let (_, _, tarball) = world(name, &Fix::new(), &Fix::new().kbf, Platform::LinuxX86_64);
    problems(image, &check(Platform::LinuxX86_64, &tarball)).expect("problems")
}

/// Asserts that a layout whose manifest or blobs are malformed is refused with the reason: two
/// manifests in the index, a digest that is not sha256, a blob the layout does not hold, a config
/// that is not JSON, a layer that is not a tar and a gzip layer that is not gzip.
#[test]
fn a_layout_with_a_malformed_index_digest_blob_or_layer_is_refused_with_the_reason() {
    let two = raw_image(
        r#"{"manifests":[{"digest":"sha256:a"},{"digest":"sha256:b"}]}"#,
        &[],
    );
    assert_eq!(
        problems_of("two", &two),
        vec!["index.json lists 2 manifests, not exactly one".to_owned()]
    );
    let md5 = raw_image(r#"{"manifests":[{"digest":"md5:abc"}]}"#, &[]);
    assert_eq!(
        problems_of("md5", &md5),
        vec![r#"manifest "md5:abc" is not a sha256 digest"#.to_owned()]
    );
    let absent = format!("sha256:{}", "0".repeat(64));
    let missing = raw_image(
        &format!(r#"{{"manifests":[{{"digest":"{absent}"}}]}}"#),
        &[],
    );
    assert_eq!(
        problems_of("absent", &missing),
        vec![format!("holds no blob for the manifest {absent}")]
    );
    let junk = b"not json".to_vec();
    let digest = format!("sha256:{}", Sha256::of(&junk));
    let bad = raw_image(
        &format!(r#"{{"manifests":[{{"digest":"{digest}"}}]}}"#),
        &[(blob_path(&junk), junk)],
    );
    let got = problems_of("junk", &bad);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with(&format!("the manifest {digest} is not JSON")),
        "{got:?}"
    );
    // A gzip-looking layer that is not gzip, and a layer that is no tar.
    for (name, layer, wanted) in [
        ("gz", vec![0x1f, 0x8b, 1, 2, 3], "not a gzip stream"),
        ("notar", vec![7u8; 700], "not a tar"),
    ] {
        let mut fix = Fix::new();
        fix.gzip = false;
        let image = fix.image();
        // Replace the first layer by `layer`, keeping its digest in the manifest by
        // rebuilding the layout around it.
        let config = serde_json::json!({"os":"linux","architecture":"amd64",
            "config":{"User":"65532","Entrypoint":["/usr/bin/kbf","--mode=frontend"]}})
        .to_string()
        .into_bytes();
        let manifest = serde_json::json!({
            "config":{"digest":format!("sha256:{}", Sha256::of(&config))},
            "layers":[{"digest":format!("sha256:{}", Sha256::of(&layer))}]})
        .to_string()
        .into_bytes();
        let index = format!(
            r#"{{"manifests":[{{"digest":"sha256:{}"}}]}}"#,
            Sha256::of(&manifest)
        );
        let raw = raw_image(
            &index,
            &[
                (blob_path(&manifest), manifest),
                (blob_path(&config), config),
                (blob_path(&layer), layer),
            ],
        );
        let got = problems_of(name, &raw);
        assert_eq!(got.len(), 1, "{name}: {got:?}");
        assert!(got[0].contains(wanted), "{name}: {got:?}");
        assert!(!image.is_empty());
    }
}

/// Asserts that an image whose layers hold no `usr/bin/kbf` is refused by name, that a
/// directory entry in the outer tar is skipped, and that a tar entry cut short is refused.
#[test]
fn an_image_with_no_kbf_is_named_and_a_truncated_tar_is_refused() {
    let mut tar = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_ustar();
    h.set_path("blobs/").expect("path");
    h.set_entry_type(tar::EntryType::Directory);
    h.set_size(0);
    h.set_mode(0o755);
    h.set_cksum();
    tar.append(&h, &b""[..]).expect("dir");
    let dir_only = tar.into_inner().expect("tar");
    assert_eq!(
        problems_of("dir", &dir_only),
        vec!["holds no index.json: not an OCI image layout".to_owned()]
    );
    let full = Fix::new().image();
    let cut = &full[..700];
    let got = problems_of("cut", cut);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("is not an OCI image tar"), "{got:?}");
    // Cut inside the body of the first entry: its header is whole, its bytes are not.
    let got = problems_of("cut-body", &full[..520]);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("is not an OCI image tar: "), "{got:?}");

    let layer = tar_of(&[("etc/other", 0o644, b"x")]);
    let config = serde_json::json!({"os":"linux","architecture":"amd64",
        "config":{"User":"65532","Entrypoint":["/usr/bin/kbf","--mode=frontend"]}})
    .to_string()
    .into_bytes();
    let manifest = serde_json::json!({
        "config":{"digest":format!("sha256:{}", Sha256::of(&config))},
        "layers":[{"digest":format!("sha256:{}", Sha256::of(&layer))}]})
    .to_string()
    .into_bytes();
    let index = format!(
        r#"{{"manifests":[{{"digest":"sha256:{}"}}]}}"#,
        Sha256::of(&manifest)
    );
    let raw = raw_image(
        &index,
        &[
            (blob_path(&manifest), manifest),
            (blob_path(&config), config),
            (blob_path(&layer), layer),
        ],
    );
    assert_eq!(
        problems_of("nokbf", &raw),
        vec!["holds no usr/bin/kbf".to_owned()]
    );
}
