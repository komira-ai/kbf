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
    /// Where the layer holds the program.
    program: &'static str,
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
    /// Layers of a base image (plain tars) below the layer that holds the program; with any,
    /// that layer holds the program alone, as a `COPY` on top of a base leaves it.
    base_layers: Vec<Vec<u8>>,
    /// More files in the layer that holds the program (path, mode, bytes), when it sits on a base.
    top_extra: Vec<(&'static str, u32, Vec<u8>)>,
    /// More entries in that layer that are not regular files: (path, type, link target).
    top_entries: Vec<(&'static str, tar::EntryType, &'static str)>,
    /// Directories `usr/` and `usr/bin/` in that layer first, as BuildKit writes a `COPY`.
    top_dirs: bool,
}

impl Fix {
    fn new() -> Fix {
        Fix {
            kbf: b"\x7fELF the kbf built for the commit".to_vec(),
            kbf_mode: 0o755,
            program: "usr/bin/kbf",
            arch: "amd64",
            os: "linux",
            user: "65532:65532",
            entrypoint: vec!["/usr/bin/kbf", "--mode=frontend"],
            gzip: true,
            later: Vec::new(),
            spoil_layer: false,
            no_index: false,
            base_layers: Vec::new(),
            top_extra: Vec::new(),
            top_entries: Vec::new(),
            top_dirs: false,
        }
    }

    /// A layer as the image carries it: gzip or plain.
    fn packed(&self, layer: &[u8]) -> Vec<u8> {
        if self.gzip {
            gzip(layer)
        } else {
            layer.to_vec()
        }
    }

    /// The base the image's `base_layers` stand for: what the pinned manifest would say.
    fn base(&self) -> Base {
        Base {
            digest: "sha256:base".to_owned(),
            layers: self
                .base_layers
                .iter()
                .map(|l| format!("sha256:{}", Sha256::of(&self.packed(l))))
                .collect(),
        }
    }

    /// The image: an OCI layout in a tar, as buildx writes one.
    fn image(&self) -> Vec<u8> {
        let mut layers = self.base_layers.clone();
        if self.base_layers.is_empty() {
            layers.push(tar_of(&[
                ("etc/ssl/certs/ca-certificates.crt", 0o644, b"roots"),
                (self.program, self.kbf_mode, &self.kbf),
            ]));
        } else {
            let mut top: Vec<(&str, u32, &[u8])> = vec![(self.program, self.kbf_mode, &self.kbf)];
            top.extend(
                self.top_extra
                    .iter()
                    .map(|(p, m, b)| (*p, *m, b.as_slice())),
            );
            let mut tar = tar::Builder::new(Vec::new());
            let mut entry =
                |path: &str, kind: tar::EntryType, mode: u32, link: &str, body: &[u8]| {
                    let mut h = tar::Header::new_ustar();
                    h.set_path(path).expect("path");
                    h.set_entry_type(kind);
                    h.set_size(body.len() as u64);
                    h.set_mode(mode);
                    if !link.is_empty() {
                        h.set_link_name(link).expect("link");
                    }
                    h.set_cksum();
                    tar.append(&h, body).expect("append");
                };
            if self.top_dirs {
                entry("usr/", tar::EntryType::Directory, 0o755, "", b"");
                entry("usr/bin/", tar::EntryType::Directory, 0o755, "", b"");
            }
            for (path, mode, bytes) in &top {
                entry(path, tar::EntryType::Regular, *mode, "", bytes);
            }
            for (path, kind, link) in &self.top_entries {
                entry(path, *kind, 0o755, link, b"");
            }
            layers.push(tar.into_inner().expect("tar"));
        }
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

/// The file name of the frontend image asset.
fn file_name(commit: &str, platform: Platform) -> String {
    Image::Frontend.file_name(commit, platform)
}

fn check(platform: Platform, tarball: &Path) -> Check<'_> {
    Check {
        image: Image::Frontend,
        platform,
        reference: Some(Reference::KbfTarball(tarball)),
        base: None,
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

/// A coordinator image: the program at its own path, its own entrypoint.
fn coordinator() -> Fix {
    let mut fix = Fix::new();
    fix.kbf = b"\x7fELF the kbf-coordinator built for the commit".to_vec();
    fix.program = "usr/bin/kbf-coordinator";
    fix.entrypoint = vec!["/usr/bin/kbf-coordinator"];
    fix
}

/// The problems of `fix` as the coordinator image for x86_64, held to `reference` when given.
fn coordinator_problems(name: &str, fix: &Fix, reference: Option<&[u8]>) -> Vec<String> {
    let dir = scratch(name);
    let binary = dir.join("kbf-coordinator");
    std::fs::write(&binary, reference.unwrap_or_default()).expect("binary");
    let c = Check {
        image: Image::Coordinator,
        platform: Platform::LinuxX86_64,
        reference: reference.map(|_| Reference::Binary(&binary)),
        base: None,
    };
    problems(&fix.image(), &c).expect("problems")
}

/// Asserts that a coordinator image holding the built `kbf-coordinator`, run as a non-root
/// user with the coordinator's entrypoint, is accepted and written as
/// `kbf-coordinator-image-<commit>-<platform>.oci.tar` with its line in the sums, and that
/// one holding other bytes, run as root or started as `kbf` is refused with its own line.
#[test]
fn a_coordinator_image_of_the_built_binary_is_accepted_and_another_is_refused() {
    let fix = coordinator();
    let dir = scratch("coordinator-good");
    let binary = dir.join("kbf-coordinator");
    std::fs::write(&binary, &fix.kbf).expect("binary");
    let image = dir.join("image.oci.tar");
    std::fs::write(&image, fix.image()).expect("image");
    let c = Check {
        image: Image::Coordinator,
        platform: Platform::LinuxX86_64,
        reference: Some(Reference::Binary(&binary)),
        base: None,
    };
    let out = dir.join("dist");
    let printed = asset(&image, COMMIT, &Mode::Add(&out), &c).expect("asset");
    let name = format!("kbf-coordinator-image-{COMMIT}-linux-x86_64.oci.tar");
    assert_eq!(
        std::fs::read(out.join(&name)).expect("asset"),
        std::fs::read(&image).expect("image")
    );
    assert_eq!(printed.first(), Some(&format!("packaged {name}:")));
    let sums = std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums");
    assert!(sums.ends_with(&format!("  {name}\n")), "{sums}");

    let good = fix.kbf.clone();
    assert!(coordinator_problems("coordinator-ok", &fix, Some(&good)).is_empty());

    let mut other = coordinator();
    *other.kbf.last_mut().expect("byte") ^= 1;
    let got = coordinator_problems("coordinator-differs", &other, Some(&good));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("usr/bin/kbf-coordinator (sha256:")
            && got[0].contains("is not the built kbf-coordinator"),
        "{got:?}"
    );

    for user in ["0", "root", "0:0", "root:root"] {
        let mut root = coordinator();
        root.user = user;
        assert_eq!(
            coordinator_problems("coordinator-root", &root, Some(&good)),
            vec![format!(
                "config runs as user {user:?}, which is root or unset"
            )],
            "{user:?}"
        );
    }

    let mut as_kbf = coordinator();
    as_kbf.entrypoint = vec!["/usr/bin/kbf", "--mode=frontend"];
    let got = coordinator_problems("coordinator-entry", &as_kbf, Some(&good));
    assert_eq!(
        got,
        vec![
            "entrypoint is [\"/usr/bin/kbf\", \"--mode=frontend\"], not [\"/usr/bin/kbf-coordinator\"]"
                .to_owned()
        ]
    );
}

/// Asserts that the coordinator's entrypoint is compared whole: the right program with one
/// more trailing argument is refused, not accepted as a prefix match.
#[test]
fn a_coordinator_entrypoint_with_an_extra_argument_is_refused() {
    let mut fix = coordinator();
    fix.entrypoint = vec!["/usr/bin/kbf-coordinator", "--extra"];
    let got = coordinator_problems("coordinator-extra-arg", &fix, None);
    assert_eq!(
        got,
        vec![
            "entrypoint is [\"/usr/bin/kbf-coordinator\", \"--extra\"], not [\"/usr/bin/kbf-coordinator\"]"
                .to_owned()
        ]
    );
}

/// Asserts that the frontend image does not pass as the coordinator's (no
/// `usr/bin/kbf-coordinator`) nor the coordinator's as the frontend's, and that checking a
/// coordinator image with no reference (the verify job) still makes the structural checks.
#[test]
fn each_image_is_refused_as_the_other_and_a_coordinator_without_a_file_is_checked_alone() {
    let got = coordinator_problems("frontend-as-coordinator", &Fix::new(), None);
    assert!(
        got.contains(&"holds no usr/bin/kbf-coordinator".to_owned()),
        "{got:?}"
    );
    assert!(
        got.iter().any(|g| g.starts_with("entrypoint is")),
        "{got:?}"
    );

    let fix = coordinator();
    let (_, image, tarball) = world(
        "coordinator-as-frontend",
        &fix,
        &fix.kbf,
        Platform::LinuxX86_64,
    );
    let got = problems(
        &std::fs::read(&image).expect("image"),
        &check(Platform::LinuxX86_64, &tarball),
    )
    .expect("problems");
    assert!(got.contains(&"holds no usr/bin/kbf".to_owned()), "{got:?}");

    assert!(coordinator_problems("coordinator-alone", &coordinator(), None).is_empty());
    let mut root = coordinator();
    root.user = "root";
    assert_eq!(
        coordinator_problems("coordinator-alone-root", &root, None).len(),
        1
    );
}

/// A task image: two base layers, then the program alone at its own path, its own entrypoint.
fn task() -> Fix {
    let mut fix = Fix::new();
    fix.kbf = b"\x7fELF the kbf-task built for the commit".to_vec();
    fix.program = "usr/bin/kbf-task";
    fix.entrypoint = vec!["/usr/bin/kbf-task"];
    fix.base_layers = vec![
        tar_of(&[("etc/os-release", 0o644, b"ubuntu 24.04")]),
        tar_of(&[("usr/lib/libz.so.1", 0o644, b"libz")]),
    ];
    fix
}

/// The problems of `fix` as the task image for arm64, held to `reference` when given.
fn task_problems(name: &str, fix: &Fix, reference: Option<&[u8]>) -> Vec<String> {
    let dir = scratch(name);
    let binary = dir.join("kbf-task");
    std::fs::write(&binary, reference.unwrap_or_default()).expect("binary");
    let base = fix.base();
    let c = Check {
        image: Image::Task,
        platform: Platform::LinuxX86_64,
        reference: reference.map(|_| Reference::Binary(&binary)),
        base: Some(&base),
    };
    problems(&fix.image(), &c).expect("problems")
}

/// Asserts that a task image holding the built `kbf-task`, run as a non-root user with the
/// task entrypoint, is accepted and written as `kbf-task-image-<commit>-<platform>.oci.tar`
/// with its line in the sums, that other bytes, root or another entrypoint are refused, that
/// a coordinator image is not a task image, and that with no reference only the structure is
/// checked.
#[test]
fn a_task_image_of_the_built_binary_is_accepted_and_another_is_refused() {
    let fix = task();
    let dir = scratch("task-good");
    let binary = dir.join("kbf-task");
    std::fs::write(&binary, &fix.kbf).expect("binary");
    let image = dir.join("image.oci.tar");
    std::fs::write(&image, fix.image()).expect("image");
    let base = fix.base();
    let c = Check {
        image: Image::Task,
        platform: Platform::LinuxX86_64,
        reference: Some(Reference::Binary(&binary)),
        base: Some(&base),
    };
    let out = dir.join("dist");
    let printed = asset(&image, COMMIT, &Mode::Add(&out), &c).expect("asset");
    let name = format!("kbf-task-image-{COMMIT}-linux-x86_64.oci.tar");
    assert_eq!(
        std::fs::read(out.join(&name)).expect("asset"),
        std::fs::read(&image).expect("image")
    );
    assert_eq!(printed.first(), Some(&format!("packaged {name}:")));
    assert_eq!(Image::named("kbf-task-image"), Some(Image::Task));

    let good = fix.kbf.clone();
    assert!(task_problems("task-ok", &fix, Some(&good)).is_empty());
    assert!(task_problems("task-alone", &fix, None).is_empty());

    let mut other = task();
    other.kbf = b"\x7fELF another".to_vec();
    let got = task_problems("task-differs", &other, Some(&good));
    assert!(
        got.len() == 1 && got[0].contains("is not the built kbf-task"),
        "{got:?}"
    );
    let mut root = task();
    root.user = "root";
    assert_eq!(task_problems("task-root", &root, Some(&good)).len(), 1);
    let mut wrong = task();
    wrong.entrypoint = vec!["/usr/bin/kbf-coordinator"];
    let got = task_problems("task-entry", &wrong, Some(&good));
    assert!(
        got.contains(
            &"entrypoint is [\"/usr/bin/kbf-coordinator\"], not [\"/usr/bin/kbf-task\"]".to_owned()
        ),
        "{got:?}"
    );
    let got = task_problems("coordinator-as-task", &coordinator(), None);
    assert!(
        got.contains(&"holds no usr/bin/kbf-task".to_owned()),
        "{got:?}"
    );
}

/// The problems of the task image `fix`, held to `base` (not the one `fix` carries).
fn task_against(name: &str, fix: &Fix, base: &Base) -> Vec<String> {
    let dir = scratch(name);
    let binary = dir.join("kbf-task");
    std::fs::write(&binary, &fix.kbf).expect("binary");
    let c = Check {
        image: Image::Task,
        platform: Platform::LinuxX86_64,
        reference: Some(Reference::Binary(&binary)),
        base: Some(base),
    };
    problems(&fix.image(), &c).expect("problems")
}

fn digests(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| format!("sha256:{n}")).collect()
}

/// Asserts the layer rule on its own: the base's layers in order and exactly one more. Catches:
/// a check that compares counts only, one that ignores order, one that lets a missing,
/// a second or an absent top layer through, and a message that does not say which layer.
#[test]
fn the_layers_must_be_the_bases_in_order_and_exactly_one_more() {
    let base = digests(&["a", "b"]);
    assert!(layer_problems(&digests(&["a", "b", "top"]), &base).is_empty());
    assert_eq!(
        layer_problems(&digests(&["a", "b"]), &base),
        vec!["the image has no layer above its base: the kbf-task layer is missing".to_owned()]
    );
    assert_eq!(
        layer_problems(&digests(&["a", "b", "top", "more"]), &base),
        vec!["the image has 2 layers above its base, not exactly one".to_owned()]
    );
    assert_eq!(
        layer_problems(&digests(&["a"]), &base),
        vec!["the image has 1 layers, fewer than the 2 of its base".to_owned()]
    );
    assert_eq!(
        layer_problems(&digests(&["a", "changed", "top"]), &base),
        vec!["layer 2 is sha256:changed, the base's is sha256:b".to_owned()]
    );
    assert_eq!(
        layer_problems(&digests(&["b", "a", "top"]), &base).len(),
        2,
        "swapped base layers are two mismatches"
    );
    // A base of no layers: the one layer is the kbf-task layer alone.
    assert!(layer_problems(&digests(&["top"]), &[]).is_empty());
}

/// Asserts that a task image of the base's layers and one more is accepted, and that an image
/// with a changed base layer, a reordered one, an extra layer, a missing top layer, a missing
/// base layer, or other files in the top layer than `usr/bin/kbf-task` is refused. Catches:
/// the base check not run on the task image, and a top layer that carries more than the binary.
#[test]
fn a_task_image_must_sit_on_its_base_with_one_layer_of_the_binary() {
    let fix = task();
    let base = fix.base();
    assert!(task_against("layers-good", &fix, &base).is_empty());

    let mut changed = task();
    changed.base_layers[0] = tar_of(&[("etc/os-release", 0o644, b"ubuntu 99")]);
    let got = task_against("layers-changed", &changed, &base);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("layer 1 is sha256:") && got[0].contains("the base's is sha256:"),
        "{got:?}"
    );

    let mut reordered = task();
    reordered.base_layers.reverse();
    assert_eq!(task_against("layers-order", &reordered, &base).len(), 2);

    let mut extra = task();
    extra.later = vec![("usr/bin/other", 0o755, b"x".to_vec())];
    assert_eq!(
        task_against("layers-extra", &extra, &base),
        vec!["the image has 2 layers above its base, not exactly one".to_owned()]
    );

    let mut missing = task();
    missing.base_layers.pop();
    let got = task_against("layers-missing-base", &missing, &base);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("layer 2 is sha256:"), "{got:?}");

    // The base is the whole image: nothing sits on top of it.
    let mut all = base.layers.clone();
    let top = tar_of(&[(fix.program, 0o755, &fix.kbf)]);
    all.push(format!("sha256:{}", Sha256::of(&fix.packed(&top))));
    let whole = Base {
        digest: base.digest.clone(),
        layers: all,
    };
    let got = task_against("layers-none-on-top", &fix, &whole);
    assert!(
        got.contains(
            &"the image has no layer above its base: the kbf-task layer is missing".to_owned()
        ),
        "{got:?}"
    );

    let mut crowded = task();
    crowded.top_extra = vec![("etc/passwd", 0o644, b"root:x:0:0".to_vec())];
    assert_eq!(
        task_against("layers-crowded-top", &crowded, &base),
        vec!["the layer above the base holds [\"etc/passwd\"] besides usr/bin/kbf-task".to_owned()]
    );
}

/// The two pinned manifests, as the Dockerfile names them: (platform, file text, arch).
fn pinned() -> [(Platform, &'static [u8], &'static str); 2] {
    [
        (Platform::LinuxX86_64, BASE_LINUX_X86_64, "amd64"),
        (Platform::LinuxArm64, BASE_LINUX_ARM64, "arm64"),
    ]
}

const DOCKERFILE: &str = include_str!("../../../../packaging/container/kbf-task/Dockerfile");

/// Asserts that the committed base manifests are what the Dockerfile pins: each file's sha256
/// is the digest on that architecture's `FROM` line, it is an image manifest (not an index) with
/// layers, and only the task image has a base. Catches: a base bumped in the Dockerfile and not
/// in the committed manifest (the layer check would hold the image to the old base), a manifest
/// edited by hand, and an index digest pinned where the release needs a per-platform manifest.
#[test]
fn the_dockerfile_pins_the_committed_base_manifests() {
    for (platform, bytes, arch) in pinned() {
        let base = Image::Task
            .base(platform)
            .expect("base")
            .expect("the task image has a base");
        assert_eq!(base.digest, format!("sha256:{}", Sha256::of(bytes)));
        assert!(!base.layers.is_empty());
        let from = format!(
            "FROM --platform=linux/{arch} docker.io/library/ubuntu@{} AS base-{arch}\n",
            base.digest
        );
        assert!(DOCKERFILE.contains(&from), "{from}");
        for image in [Image::Frontend, Image::Coordinator] {
            assert_eq!(image.base(platform).expect("base"), None);
        }
    }
    assert!(DOCKERFILE.contains("FROM base-${TARGETARCH}\n"));
    assert!(DOCKERFILE.contains("USER 65532:65532\n"));
    assert!(DOCKERFILE.contains("ENTRYPOINT [\"/usr/bin/kbf-task\"]\n"));
    // The kbf-task layer is the last instruction that adds files.
    let copy = DOCKERFILE.find("\nCOPY ").expect("COPY");
    assert!(DOCKERFILE[copy + 1..].matches("COPY ").count() == 1);
}

/// Asserts what a pinned manifest must be: its digest is the sha256 of its bytes, its layers
/// are the digests it lists, and an index, a manifest without layers, a layer that is not a
/// sha256 digest, and JSON that is not a manifest are each refused with the reason.
#[test]
fn a_base_is_a_manifest_whose_digest_is_the_hash_of_its_bytes() {
    let text = br#"{"schemaVersion":2,"layers":[{"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},{"digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]}"#;
    let base = Base::parse(text).expect("base");
    assert_eq!(base.digest, format!("sha256:{}", Sha256::of(text)));
    assert_eq!(
        base.layers,
        vec![
            format!("sha256:{}", "a".repeat(64)),
            format!("sha256:{}", "b".repeat(64))
        ]
    );
    for (bytes, why) in [
        (
            &br#"{"manifests":[{"digest":"sha256:aa"}]}"#[..],
            "an image index",
        ),
        (br#"{"layers":[]}"#, "no layers"),
        (
            br#"{"layers":[{"digest":"md5:abc"}]}"#,
            "not a sha256 digest",
        ),
        (br#"{"layers":[{"size":3}]}"#, "not a sha256 digest"),
        (
            br#"{"layers":[{"digest":"sha256:ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ"}]}"#,
            "not a sha256 digest",
        ),
        (b"not json", "not JSON"),
    ] {
        let err = Base::parse(bytes).expect_err(why);
        assert!(err.contains(why), "{err:?} for {why}");
    }
}

/// The blob named `name` in the layout `image`.
fn blob_named(image: &[u8], name: &str) -> Vec<u8> {
    files(image)
        .expect("layout")
        .into_iter()
        .find(|(p, _, _)| p == name)
        .expect("blob")
        .2
}

/// Asserts that the facts of a task image record the manifest digest `index.json` names (the
/// sha256 of the manifest blob), the config digest, every layer in order, the base's digest,
/// the commit and the platform, as the same bytes every time, and that an index naming two
/// manifests, or a manifest blob that does not hash to its name, gives no facts.
#[test]
fn the_facts_name_the_manifest_digest_from_index_json_and_the_base() {
    let fix = task();
    let image = fix.image();
    let base = fix.base();
    let text =
        facts_text(&image, Image::Task, COMMIT, Platform::LinuxX86_64, &base).expect("facts");
    assert_eq!(
        text,
        facts_text(&image, Image::Task, COMMIT, Platform::LinuxX86_64, &base).expect("facts")
    );
    assert!(text.ends_with('\n'));
    let facts: Value = serde_json::from_str(&text).expect("JSON");
    let index: Value = serde_json::from_slice(&blob_named(&image, "index.json")).expect("index");
    let named = index["manifests"][0]["digest"].as_str().expect("digest");
    let manifest = blob_named(
        &image,
        &format!("blobs/sha256/{}", &named["sha256:".len()..]),
    );
    assert_eq!(named, format!("sha256:{}", Sha256::of(&manifest)));
    assert_eq!(facts["manifest"], named);
    let manifest: Value = serde_json::from_slice(&manifest).expect("manifest");
    assert_eq!(facts["config"], manifest["config"]["digest"]);
    assert_eq!(
        facts["layers"],
        serde_json::json!(
            manifest["layers"]
                .as_array()
                .expect("layers")
                .iter()
                .map(|l| l["digest"].clone())
                .collect::<Vec<_>>()
        )
    );
    assert_eq!(facts["layers"].as_array().expect("layers").len(), 3);
    assert_eq!(facts["base"], "sha256:base");
    assert_eq!(facts["commit"], COMMIT);
    assert_eq!(facts["platform"], "linux-x86_64");
    assert_eq!(facts["image"], "kbf-task-image");
    assert_eq!(facts["schema"], 1);

    let two = raw_image(
        r#"{"manifests":[{"digest":"sha256:aa"},{"digest":"sha256:bb"}]}"#,
        &[],
    );
    let err = facts_text(&two, Image::Task, COMMIT, Platform::LinuxX86_64, &base).expect_err("two");
    assert!(err.to_string().contains("not exactly one"), "{err}");
    let hex = Sha256::of(b"the manifest").to_string();
    let wrong = raw_image(
        &format!(r#"{{"manifests":[{{"digest":"sha256:{hex}"}}]}}"#),
        &[(format!("blobs/sha256/{hex}"), b"other bytes".to_vec())],
    );
    let err = facts_text(&wrong, Image::Task, COMMIT, Platform::LinuxX86_64, &base)
        .expect_err("a manifest blob that does not hash to its name");
    assert!(err.to_string().contains("has another digest"), "{err}");
}

/// Asserts the names of the facts: the task image has facts named after it, the facts name its
/// image back, and the frontend and coordinator images, other commits, the Mac and look-alikes
/// have none. Catches: a facts name any file ending `.facts.json` could satisfy.
#[test]
fn the_facts_file_is_named_after_its_task_image() {
    let image = format!("kbf-task-image-{COMMIT}-linux-arm64.oci.tar");
    let facts = format!("kbf-task-image-{COMMIT}-linux-arm64.facts.json");
    assert_eq!(Image::Task.facts_name(COMMIT, Platform::LinuxArm64), facts);
    assert_eq!(facts_of(&image), Some(facts.clone()));
    assert_eq!(image_of(&facts), Some(image));
    assert!(is_facts_of(&facts, COMMIT));
    assert!(!is_facts_of(
        &facts,
        "89abcdef0123456789abcdef0123456789abcdef"
    ));
    for not in [
        format!("kbf-frontend-image-{COMMIT}-linux-arm64.oci.tar"),
        format!("kbf-coordinator-image-{COMMIT}-linux-arm64.oci.tar"),
        format!("kbf-task-image-{COMMIT}-darwin-arm64.oci.tar"),
        format!("kbf-task-image-{COMMIT}-linux-arm64.tar.gz"),
        "kbf-task-image-nothex-linux-arm64.oci.tar".to_owned(),
        format!("kbf-task-image-{COMMIT}.oci.tar"),
        format!("kbf-task-image-{COMMIT}-linux-riscv64.oci.tar"),
    ] {
        assert_eq!(facts_of(&not), None, "{not}");
    }
    for not in [
        format!("kbf-frontend-image-{COMMIT}-linux-arm64.facts.json"),
        format!("kbf-task-image-{COMMIT}-darwin-arm64.facts.json"),
        format!("kbf-task-{COMMIT}-linux-arm64.facts.json"),
        format!("kbf-{COMMIT}-linux-arm64.version.json"),
        "kbf-task-image-nothex-linux-arm64.facts.json".to_owned(),
    ] {
        assert_eq!(image_of(&not), None, "{not}");
        assert!(!is_facts_of(&not, COMMIT), "{not}");
    }
}

/// Asserts that adding a task image writes its facts beside it and lists them in the sums after
/// the image, that `--verify-only` accepts the image with the facts it wrote, and refuses it
/// with no facts beside it, with facts of another image (changed manifest digest), and with
/// facts altered by a byte. Catches: facts that are written but never compared with the image
/// the attestation covers, which would let a release attest a digest the image does not have.
#[test]
fn a_task_image_is_listed_with_its_facts_and_verified_against_them() {
    let fix = task();
    let dir = scratch("facts-asset");
    let binary = dir.join("kbf-task");
    std::fs::write(&binary, &fix.kbf).expect("binary");
    let image = dir.join("image.oci.tar");
    std::fs::write(&image, fix.image()).expect("image");
    let base = fix.base();
    let c = Check {
        image: Image::Task,
        platform: Platform::LinuxX86_64,
        reference: Some(Reference::Binary(&binary)),
        base: Some(&base),
    };
    let out = dir.join("dist");
    let printed = asset(&image, COMMIT, &Mode::Add(&out), &c).expect("asset");
    let name = format!("kbf-task-image-{COMMIT}-linux-x86_64.oci.tar");
    let facts_name = format!("kbf-task-image-{COMMIT}-linux-x86_64.facts.json");
    let facts = std::fs::read(out.join(&facts_name)).expect("facts");
    assert_eq!(
        facts,
        facts_text(
            &fix.image(),
            Image::Task,
            COMMIT,
            Platform::LinuxX86_64,
            &base
        )
        .expect("text")
        .into_bytes()
    );
    let sums = std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums");
    let want = format!(
        "{}  {name}\n{}  {facts_name}\n",
        Sha256::of(&fix.image()),
        Sha256::of(&facts)
    );
    assert_eq!(sums, want);
    assert!(
        printed.contains(&format!("{}  {facts_name}", Sha256::of(&facts))),
        "{printed:?}"
    );

    // The verify job: the signed files in one directory.
    let verify = |dir: &Path| asset(&dir.join(&name), COMMIT, &Mode::VerifyOnly, &c);
    verify(&out).expect("the image with its facts verifies");
    std::fs::remove_file(out.join(&facts_name)).expect("remove");
    let err = verify(&out).expect_err("no facts").to_string();
    assert!(
        err.contains(&facts_name) && err.contains("no facts"),
        "{err}"
    );

    // Facts of another build of the image.
    let other = {
        let mut f = task();
        f.base_layers
            .push(tar_of(&[("usr/share/x", 0o644, b"another base layer")]));
        f
    };
    let other_base = other.base();
    let elsewhere = facts_text(
        &other.image(),
        Image::Task,
        COMMIT,
        Platform::LinuxX86_64,
        &other_base,
    )
    .expect("other facts");
    std::fs::write(out.join(&facts_name), elsewhere).expect("facts");
    let err = verify(&out).expect_err("another image's facts").to_string();
    assert!(err.contains("are not the facts of the image"), "{err}");

    let mut flipped = facts.clone();
    *flipped.last_mut().expect("byte") ^= 1;
    std::fs::write(out.join(&facts_name), &flipped).expect("facts");
    assert!(verify(&out).is_err());
    std::fs::write(out.join(&facts_name), &facts).expect("facts");
    verify(&out).expect("restored");
}

/// Asserts that a task image listed already is not added twice, and that a coordinator image
/// (no base) gets no facts: its sums have the image's line alone.
#[test]
fn only_an_image_with_a_base_gets_facts() {
    let fix = coordinator();
    let dir = scratch("facts-none");
    let binary = dir.join("kbf-coordinator");
    std::fs::write(&binary, &fix.kbf).expect("binary");
    let image = dir.join("image.oci.tar");
    std::fs::write(&image, fix.image()).expect("image");
    let c = Check {
        image: Image::Coordinator,
        platform: Platform::LinuxX86_64,
        reference: Some(Reference::Binary(&binary)),
        base: None,
    };
    let out = dir.join("dist");
    asset(&image, COMMIT, &Mode::Add(&out), &c).expect("asset");
    let mut names: Vec<String> = std::fs::read_dir(&out)
        .expect("dist")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            format!("SHA256SUMS.linux-x86_64"),
            format!("kbf-coordinator-image-{COMMIT}-linux-x86_64.oci.tar"),
        ]
    );
}

/// Asserts the failures around the facts file that a good run does not reach: a pinned manifest
/// that is not one is named with the image and platform, facts already listed stop the run, a
/// facts path that cannot be read is an error and not a pass, and the verify job's check with no
/// build file (no reference) says what it checked.
#[test]
fn the_facts_failures_name_what_is_wrong() {
    let err = Image::Task
        .pinned(Platform::LinuxArm64, b"{}")
        .expect_err("not a manifest")
        .to_string();
    assert_eq!(
        err,
        "the pinned base manifest of the task image for linux-arm64 lists no layers"
    );

    let fix = task();
    let base = fix.base();
    let dir = scratch("facts-failures");
    let binary = dir.join("kbf-task");
    std::fs::write(&binary, &fix.kbf).expect("binary");
    let image = dir.join("image.oci.tar");
    std::fs::write(&image, fix.image()).expect("image");
    let c = Check {
        image: Image::Task,
        platform: Platform::LinuxX86_64,
        reference: Some(Reference::Binary(&binary)),
        base: Some(&base),
    };
    let facts_name = format!("kbf-task-image-{COMMIT}-linux-x86_64.facts.json");
    let out = dir.join("dist");
    std::fs::create_dir_all(&out).expect("dist");
    let listed = format!("{}  {facts_name}\n", Sha256::of(b"earlier"));
    std::fs::write(out.join("SHA256SUMS.linux-x86_64"), &listed).expect("sums");
    let err = asset(&image, COMMIT, &Mode::Add(&out), &c)
        .expect_err("listed")
        .to_string();
    assert!(
        err.ends_with(&format!("already lists {facts_name}")),
        "{err}"
    );

    // The signed directory: the image under its name, a directory where the facts should be.
    let signed = dir.join("signed");
    std::fs::create_dir_all(signed.join(&facts_name)).expect("dir");
    let name = format!("kbf-task-image-{COMMIT}-linux-x86_64.oci.tar");
    std::fs::copy(&image, signed.join(&name)).expect("copy");
    let err = asset(&signed.join(&name), COMMIT, &Mode::VerifyOnly, &c)
        .expect_err("unreadable facts")
        .to_string();
    assert!(err.starts_with("read "), "{err}");

    // With no reference only the structure is checked, as the verify job does.
    let alone = Check {
        reference: None,
        ..c
    };
    std::fs::remove_dir(signed.join(&facts_name)).expect("rmdir");
    let facts = facts_text(
        &fix.image(),
        Image::Task,
        COMMIT,
        Platform::LinuxX86_64,
        &base,
    )
    .expect("facts");
    std::fs::write(signed.join(&facts_name), facts).expect("facts");
    let printed = asset(&signed.join(&name), COMMIT, &Mode::VerifyOnly, &alone).expect("verified");
    assert_eq!(
        printed,
        vec![
            format!("{name}: a linux-x86_64 task image"),
            format!("{facts_name}: the facts of the image"),
        ]
    );
}

/// Asserts the top layer may hold the entries BuildKit writes for the copy (the directories `usr/`
/// and `usr/bin/` and the program at 0755), and refuses a symlink, a hard link, a device, a fifo
/// and a directory other than those two, each named with its type. Catches: an entry that is not a
/// regular file passing unseen because only regular files are listed.
#[test]
fn the_top_layer_may_hold_only_the_program_and_its_directories() {
    let base = task().base();
    let mut good = task();
    good.top_dirs = true;
    assert!(task_against("top-dirs", &good, &base).is_empty());
    for (name, path, kind, link, shown) in [
        (
            "sym",
            "usr/bin/sh",
            tar::EntryType::Symlink,
            "/bin/busybox",
            "usr/bin/sh (Symlink)",
        ),
        (
            "hard",
            "usr/bin/other",
            tar::EntryType::Link,
            "usr/bin/kbf-task",
            "usr/bin/other (Link)",
        ),
        (
            "dir",
            "etc/",
            tar::EntryType::Directory,
            "",
            "etc (Directory)",
        ),
        (
            "dev",
            "dev/null",
            tar::EntryType::Char,
            "",
            "dev/null (Char)",
        ),
        (
            "fifo",
            "run/pipe",
            tar::EntryType::Fifo,
            "",
            "run/pipe (Fifo)",
        ),
    ] {
        let mut bad = task();
        bad.top_dirs = true;
        bad.top_entries = vec![(path, kind, link)];
        assert_eq!(
            task_against(&format!("top-{name}"), &bad, &base),
            vec![format!(
                "the layer above the base holds [\"{shown}\"] besides usr/bin/kbf-task"
            )],
            "{name}"
        );
    }
}
