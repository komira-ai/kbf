use std::path::PathBuf;

use super::*;
use crate::pkg_linux::Out;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const SHORT: &str = "0123456789ab";
const BINARY: &[u8] = b"\x7fELF the attested binary";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-image-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// One entry of a layer: a file with a mode and bytes, or a directory.
enum Item {
    File(&'static str, u32, Vec<u8>),
    Dir(&'static str),
}

fn file(path: &'static str, mode: u32, bytes: &[u8]) -> Item {
    Item::File(path, mode, bytes.to_vec())
}

/// The tar of `items`, as a layer of `docker save` is.
fn layer(items: &[Item]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for item in items {
        let mut h = tar::Header::new_ustar();
        match item {
            Item::File(path, mode, bytes) => {
                h.set_path(path).expect("path");
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(bytes.len() as u64);
                h.set_mode(*mode);
                h.set_cksum();
                tar.append(&h, &bytes[..]).expect("append");
            }
            Item::Dir(path) => {
                h.set_path(path).expect("path");
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                h.set_mode(0o755);
                h.set_cksum();
                tar.append(&h, std::io::empty()).expect("append");
            }
        }
    }
    tar.into_inner().expect("tar")
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).expect("gzip");
    gz.finish().expect("gzip")
}

/// What a good image is made of, each of which a test may change.
struct Fix {
    tags: Vec<String>,
    os: &'static str,
    arch: &'static str,
    user: &'static str,
    entrypoint: Vec<&'static str>,
    env: Vec<&'static str>,
    label: Option<String>,
    layers: Vec<Vec<u8>>,
    /// Whether the manifest names a configuration that is in the archive.
    config_present: bool,
    /// Whether the manifest is a one-image list.
    images: usize,
}

impl Fix {
    /// A good frontend image for `linux-x86_64`.
    fn good() -> Self {
        Fix {
            tags: vec![format!("kbf-frontend:{COMMIT}")],
            os: "linux",
            arch: "amd64",
            user: "10001",
            entrypoint: vec!["/usr/bin/kbf"],
            env: vec!["PATH=/usr/local/bin:/usr/bin"],
            label: Some(COMMIT.to_owned()),
            layers: vec![
                layer(&[file("etc/os-release", 0o644, b"ubuntu")]),
                layer(&[
                    Item::Dir("etc/kbf/"),
                    Item::Dir("var/lib/kbf-server/"),
                    file("usr/bin/kbf", 0o755, BINARY),
                ]),
            ],
            config_present: true,
            images: 1,
        }
    }

    fn config(&self) -> Vec<u8> {
        let labels = self
            .label
            .as_ref()
            .map(|l| serde_json::json!({ REVISION_LABEL: l }))
            .unwrap_or_default();
        serde_json::to_vec(&serde_json::json!({
            "os": self.os,
            "architecture": self.arch,
            "config": {
                "User": self.user,
                "Entrypoint": self.entrypoint,
                "Env": self.env,
                "Labels": labels,
            },
        }))
        .expect("json")
    }

    /// The `docker save` archive.
    fn archive(&self) -> Vec<u8> {
        let mut outer = tar::Builder::new(Vec::new());
        // docker save lists the directories of its blobs too.
        let mut dir = tar::Header::new_ustar();
        dir.set_path("blobs/").expect("path");
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mode(0o755);
        dir.set_cksum();
        outer.append(&dir, std::io::empty()).expect("append");
        let mut put = |path: &str, bytes: &[u8]| {
            let mut h = tar::Header::new_ustar();
            h.set_path(path).expect("path");
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            outer.append(&h, bytes).expect("append");
        };
        let names: Vec<String> = (0..self.layers.len())
            .map(|i| format!("blobs/sha256/layer{i}"))
            .collect();
        for (name, bytes) in names.iter().zip(&self.layers) {
            put(name, bytes);
        }
        if self.config_present {
            put("blobs/sha256/config", &self.config());
        }
        let one = serde_json::json!({
            "Config": "blobs/sha256/config",
            "RepoTags": self.tags,
            "Layers": names,
        });
        let manifest = serde_json::Value::Array(vec![one; self.images]);
        put("manifest.json", manifest.to_string().as_bytes());
        outer.into_inner().expect("tar")
    }
}

/// Where the binary the image was built from is.
fn binary_file(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("kbf");
    std::fs::write(&path, BINARY).expect("binary");
    path
}

fn check(binary: &std::path::Path) -> Check<'_> {
    Check {
        image: Image::Frontend,
        platform: Platform::LinuxX86_64,
        commit: COMMIT,
        binary,
    }
}

/// The problems of `fix` as the image of the good check.
fn problems_of(name: &str, fix: &Fix) -> Vec<String> {
    let dir = scratch(name);
    let binary = binary_file(&dir);
    problems(&fix.archive(), &check(&binary)).expect("checked")
}

fn said(found: &[String], text: &str) -> bool {
    found.iter().any(|p| p.contains(text))
}

#[test]
fn a_good_image_has_no_problem() {
    assert_eq!(problems_of("good", &Fix::good()), Vec::<String>::new());
}

#[test]
fn a_user_with_a_group_and_gzip_layers_are_good() {
    let mut fix = Fix::good();
    fix.user = "10001:10001";
    fix.layers = fix.layers.iter().map(|l| gzip(l)).collect();
    assert_eq!(problems_of("gzip", &fix), Vec::<String>::new());
}

#[test]
fn the_coordinator_image_is_checked_for_its_own_binary_and_tag() {
    let dir = scratch("coordinator");
    let binary = dir.join("kbf-coordinator");
    std::fs::write(&binary, BINARY).expect("binary");
    let mut fix = Fix::good();
    fix.tags = vec![format!("kbf-coordinator:{COMMIT}")];
    fix.entrypoint = vec!["/usr/bin/kbf-coordinator"];
    fix.layers[1] = layer(&[file("usr/bin/kbf-coordinator", 0o755, BINARY)]);
    let c = Check {
        image: Image::Coordinator,
        ..check(&binary)
    };
    assert_eq!(
        problems(&fix.archive(), &c).expect("checked"),
        Vec::<String>::new()
    );
    // The frontend's tag and binary are not the coordinator's.
    let found = problems(&Fix::good().archive(), &c).expect("checked");
    assert!(said(&found, "is tagged"), "{found:?}");
    assert!(said(&found, "usr/bin/kbf-coordinator"), "{found:?}");
}

#[test]
fn an_archive_that_is_not_one_image_is_refused_alone() {
    let mut two = Fix::good();
    two.images = 2;
    let found = problems_of("two", &two);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(said(&found, "lists 2 images"), "{found:?}");
    let dir = scratch("not-a-tar");
    let binary = binary_file(&dir);
    let found = problems(&[7u8; 600], &check(&binary)).expect("checked");
    assert!(said(&found, "not a tar"), "{found:?}");
    // A tar with no manifest.json lists no image.
    let found = problems(&layer(&[file("x", 0o644, b"")]), &check(&binary)).expect("checked");
    assert!(said(&found, "lists 0 images"), "{found:?}");
}

#[test]
fn a_missing_configuration_is_named_and_nothing_after_it_is_guessed() {
    let mut fix = Fix::good();
    fix.config_present = false;
    let found = problems_of("noconfig", &fix);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(said(&found, "no image configuration"), "{found:?}");
}

#[test]
fn the_tag_must_be_the_commit_and_only_that() {
    for (name, tags) in [
        ("other", vec!["kbf-frontend:latest".to_owned()]),
        ("none", vec![]),
        (
            "two",
            vec![
                format!("kbf-frontend:{COMMIT}"),
                "kbf-frontend:latest".to_owned(),
            ],
        ),
    ] {
        let mut fix = Fix::good();
        fix.tags = tags;
        let found = problems_of(&format!("tag-{name}"), &fix);
        assert!(said(&found, "is tagged"), "{name}: {found:?}");
    }
}

#[test]
fn the_platform_user_entrypoint_environment_and_revision_are_held_to_the_rules() {
    type Change = fn(&mut Fix);
    let cases: [(&str, Change, &str); 11] = [
        ("os", |f| f.os = "windows", "says os"),
        ("arch", |f| f.arch = "arm64", "says architecture"),
        ("root", |f| f.user = "0", "runs as user"),
        ("root-named", |f| f.user = "root", "runs as user"),
        ("nobody", |f| f.user = "", "runs as user"),
        ("other-uid", |f| f.user = "1000", "runs as user"),
        (
            "entry",
            |f| f.entrypoint = vec!["/usr/bin/other"],
            "has the entrypoint",
        ),
        (
            "env",
            |f| f.env = vec!["PATH=/usr/bin", "OPERATOR_TOKEN=x"],
            "environment variable OPERATOR_TOKEN",
        ),
        (
            "nolabel",
            |f| f.label = None,
            "org.opencontainers.image.revision",
        ),
        (
            "oldlabel",
            |f| f.label = Some("f".repeat(40)),
            "org.opencontainers.image.revision",
        ),
        ("noentry", |f| f.entrypoint = vec![], "has the entrypoint"),
    ];
    for (name, change, want) in cases {
        let mut fix = Fix::good();
        change(&mut fix);
        let found = problems_of(&format!("rule-{name}"), &fix);
        assert!(said(&found, want), "{name}: {found:?}");
        assert_eq!(found.len(), 1, "{name}: only that one: {found:?}");
    }
}

#[test]
fn an_arm64_image_is_for_the_arm64_platform_only() {
    let dir = scratch("arm");
    let binary = binary_file(&dir);
    let mut fix = Fix::good();
    fix.arch = "arm64";
    let arm = Check {
        platform: Platform::LinuxArm64,
        ..check(&binary)
    };
    assert_eq!(
        problems(&fix.archive(), &arm).expect("checked"),
        Vec::<String>::new()
    );
    assert!(said(
        &problems(&Fix::good().archive(), &arm).expect("checked"),
        "says architecture"
    ));
}

#[test]
fn the_binary_must_be_there_executable_and_the_one_that_was_built() {
    let mut missing = Fix::good();
    missing.layers.pop();
    assert!(said(
        &problems_of("nobin", &missing),
        "holds no usr/bin/kbf"
    ));
    for (name, mode) in [("plain", 0o644), ("open", 0o777), ("owner", 0o700)] {
        let mut fix = Fix::good();
        fix.layers[1] = layer(&[file("usr/bin/kbf", mode, BINARY)]);
        let found = problems_of(&format!("mode-{name}"), &fix);
        assert!(said(&found, "has mode"), "{name}: {found:?}");
    }
    let mut other = Fix::good();
    other.layers[1] = layer(&[file("usr/bin/kbf", 0o755, b"another binary")]);
    let found = problems_of("other-bytes", &other);
    assert!(said(&found, "is not the bytes of"), "{found:?}");
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn a_later_layer_decides_what_the_binary_is_and_whiteouts_remove_files() {
    // The later layer's binary is the one that runs.
    let mut replaced = Fix::good();
    replaced
        .layers
        .push(layer(&[file("usr/bin/kbf", 0o755, b"swapped")]));
    assert!(said(
        &problems_of("swapped", &replaced),
        "is not the bytes of"
    ));
    let mut restored = Fix::good();
    restored
        .layers
        .insert(1, layer(&[file("usr/bin/kbf", 0o755, b"old")]));
    assert_eq!(problems_of("restored", &restored), Vec::<String>::new());
    // A file baked into a mounted directory and then whited out is not in the image.
    let baked = || file("etc/kbf/kbf.toml", 0o644, b"x");
    for (name, whiteout) in [
        ("file", file("etc/kbf/.wh.kbf.toml", 0o644, b"")),
        ("dir", file("etc/.wh.kbf", 0o644, b"")),
        ("opaque", file("etc/kbf/.wh..wh..opq", 0o644, b"")),
    ] {
        let mut fix = Fix::good();
        fix.layers.insert(1, layer(&[baked()]));
        assert!(
            said(
                &problems_of(&format!("baked-{name}"), &fix),
                "baked into the image"
            ),
            "{name}: still there before the whiteout"
        );
        fix.layers.insert(2, layer(&[whiteout]));
        assert_eq!(
            problems_of(&format!("whiteout-{name}"), &fix),
            Vec::<String>::new(),
            "{name}"
        );
    }
    // A whiteout at the root removes a file at the root.
    let mut root = Fix::good();
    root.layers.insert(
        1,
        layer(&[file("etc", 0o644, b"a file named like a directory")]),
    );
    root.layers.insert(2, layer(&[file(".wh.etc", 0o644, b"")]));
    assert_eq!(problems_of("root-whiteout", &root), Vec::<String>::new());
    let mut opaque_root = Fix::good();
    opaque_root
        .layers
        .push(layer(&[file(".wh..wh..opq", 0o644, b"")]));
    assert!(said(
        &problems_of("root-opaque", &opaque_root),
        "holds no usr/bin/kbf"
    ));
}

#[test]
fn a_setting_key_or_state_baked_into_a_mounted_directory_is_refused() {
    for path in [
        "etc/kbf/kbf.toml",
        "etc/kbf-coordinator/tls.key",
        "var/lib/kbf-server/state.db",
        "var/lib/kbf-coordinator/desired.json",
    ] {
        let mut fix = Fix::good();
        fix.layers.push(layer(&[Item::File(
            Box::leak(path.to_owned().into_boxed_str()),
            0o600,
            b"secret".to_vec(),
        )]));
        let found = problems_of(&format!("baked-{}", path.replace('/', "-")), &fix);
        assert!(
            said(&found, &format!("holds the file {path}")),
            "{path}: {found:?}"
        );
    }
}

#[test]
fn a_layer_that_is_missing_or_not_a_tar_is_a_problem() {
    let mut fix = Fix::good();
    fix.layers
        .push(b"not a tar at all, and long enough to hold a header block.......".repeat(20));
    assert!(said(&problems_of("garbage", &fix), "a layer is not a tar"));
    let mut bad_gzip = Fix::good();
    bad_gzip
        .layers
        .push([0x1f, 0x8b, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12].to_vec());
    assert!(said(&problems_of("badgz", &bad_gzip), "not a gzip stream"));
    // A manifest naming a layer the archive lacks.
    let dir = scratch("nolayer");
    let binary = binary_file(&dir);
    let fix = Fix::good();
    let saved = fix.archive();
    let mut outer = tar::Archive::new(&saved[..]);
    let mut kept = tar::Builder::new(Vec::new());
    for entry in outer.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        if entry.path().expect("path").ends_with("layer0") {
            continue;
        }
        let header = entry.header().clone();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut body).expect("body");
        kept.append(&header, &body[..]).expect("append");
    }
    let found = problems(&kept.into_inner().expect("tar"), &check(&binary)).expect("checked");
    assert!(
        said(&found, "layer blobs/sha256/layer0 is not in the archive"),
        "{found:?}"
    );
}

#[test]
fn a_binary_that_cannot_be_read_is_an_error_not_a_problem() {
    let dir = scratch("nobinary");
    let err = problems(&Fix::good().archive(), &check(&dir.join("absent"))).unwrap_err();
    assert!(err.to_string().contains("absent"), "{err}");
}

const SBOM: &[u8] = br#"{"bomFormat":"CycloneDX","specVersion":"1.5"}"#;

/// A directory with the archive, the binary and the SBOM of a good image.
fn assets(name: &str, fix: &Fix) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = scratch(name);
    let archive = dir.join("image.tar");
    std::fs::write(&archive, fix.archive()).expect("archive");
    let sbom = dir.join("sbom.json");
    std::fs::write(&sbom, SBOM).expect("sbom");
    let binary = binary_file(&dir);
    let out = dir.join("dist");
    (archive, sbom, binary, out)
}

#[test]
fn the_image_and_its_sbom_become_assets_listed_in_the_sums() {
    let (archive, sbom, binary, out) = assets("asset", &Fix::good());
    let printed = asset(&archive, &sbom, &out, &check(&binary)).expect("asset");
    let name = format!("kbf-frontend-image-{COMMIT}-linux-x86_64.tar");
    let sbom_name = format!("kbf-frontend-image-{COMMIT}-linux-x86_64.cdx.json");
    assert_eq!(
        std::fs::read(out.join(&name)).expect("asset"),
        std::fs::read(&archive).unwrap()
    );
    assert_eq!(std::fs::read(out.join(&sbom_name)).expect("sbom"), SBOM);
    let text = std::fs::read_to_string(out.join("SHA256SUMS.linux-x86_64")).expect("sums");
    let lines = sums::parse(&text).expect("sums");
    assert_eq!(
        lines.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
        [name.as_str(), sbom_name.as_str()]
    );
    assert_eq!(
        lines[0].sha256,
        Sha256::of(&std::fs::read(&archive).unwrap())
    );
    assert_eq!(lines[1].sha256, Sha256::of(SBOM));
    assert!(printed.iter().any(|l| l.contains(&name)), "{printed:?}");
    // The same image again is a second listing of the same name.
    let again = asset(&archive, &sbom, &out, &check(&binary)).unwrap_err();
    assert!(again.to_string().contains("already lists"), "{again}");
}

#[test]
fn an_sbom_already_listed_is_refused_after_the_image() {
    let (archive, sbom, binary, out) = assets("sbom-listed", &Fix::good());
    std::fs::create_dir_all(&out).expect("out");
    let sbom_name = format!("kbf-frontend-image-{COMMIT}-linux-x86_64.cdx.json");
    std::fs::write(
        out.join("SHA256SUMS.linux-x86_64"),
        format!("{}  {sbom_name}\n", Sha256::of(b"x")),
    )
    .expect("sums");
    let err = asset(&archive, &sbom, &out, &check(&binary)).unwrap_err();
    assert!(err.to_string().contains(&sbom_name), "{err}");
}

#[test]
fn a_refused_image_writes_nothing() {
    let mut fix = Fix::good();
    fix.user = "0";
    let (archive, sbom, binary, out) = assets("refused", &fix);
    let err = asset(&archive, &sbom, &out, &check(&binary)).unwrap_err();
    assert!(err.to_string().contains("runs as user"), "{err}");
    assert!(!out.exists() || std::fs::read_dir(&out).unwrap().next().is_none());
}

#[test]
fn an_sbom_that_is_not_cyclonedx_or_a_platform_that_is_not_linux_is_refused() {
    let (archive, sbom, binary, out) = assets("bad-sbom", &Fix::good());
    for text in [&b""[..], b"{}", b"[1]", br#"{"bomFormat":"SPDX"}"#] {
        std::fs::write(&sbom, text).expect("sbom");
        let err = asset(&archive, &sbom, &out, &check(&binary)).unwrap_err();
        assert!(
            err.to_string().contains("not a CycloneDX"),
            "{text:?}: {err}"
        );
    }
    std::fs::remove_file(&sbom).expect("remove");
    let err = asset(&archive, &sbom, &out, &check(&binary)).unwrap_err();
    assert!(err.to_string().contains("sbom.json"), "{err}");
    std::fs::write(&sbom, SBOM).expect("sbom");
    let mac = Check {
        platform: Platform::DarwinArm64,
        ..check(&binary)
    };
    let err = asset(&archive, &sbom, &out, &mac).unwrap_err();
    assert!(err.to_string().contains("for Linux"), "{err}");
    let err = asset(
        &archive,
        &sbom,
        &out,
        &Check {
            commit: "nope",
            ..check(&binary)
        },
    )
    .unwrap_err();
    assert!(matches!(err, Error::NotACommit(_)), "{err}");
}

#[test]
fn the_names_of_the_assets_follow_the_release_set() {
    assert_eq!(
        Image::Coordinator.file_name(COMMIT, Platform::LinuxArm64),
        format!("kbf-coordinator-image-{COMMIT}-linux-arm64.tar")
    );
    assert_eq!(
        Image::Frontend.sbom_name(COMMIT, Platform::LinuxX86_64),
        format!("kbf-frontend-image-{COMMIT}-linux-x86_64.cdx.json")
    );
    assert_eq!(Image::Coordinator.binary(), "kbf-coordinator");
    assert_eq!(Image::ALL.len(), 2);
}

/// What the model of docker answers, each of which a test may change.
struct Model {
    loaded: String,
    version: String,
    json: String,
    uid: String,
    bare_ok: bool,
    /// The first word after `docker` that fails.
    fail: Option<&'static str>,
}

impl Model {
    fn good(image: Image) -> Self {
        let binary = image.binary();
        Model {
            loaded: format!("Loaded image: {}\n", image.tag(COMMIT)),
            version: format!("{binary} 0.1.0+{SHORT}\n"),
            json: format!("{{\"commit\":\"{SHORT}\",\"profile\":\"release\"}}\n"),
            uid: "10001\n".to_owned(),
            bare_ok: false,
            fail: None,
        }
    }

    /// The answer to `docker <args>`.
    fn answer(&self, args: &[&str]) -> Out {
        let ok = |stdout: &str| Out {
            ok: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        };
        let failed = Out {
            ok: false,
            stdout: String::new(),
            stderr: "boom\n".to_owned(),
        };
        if self.fail == Some(args[0]) {
            return failed;
        }
        match args[0] {
            "load" => ok(&self.loaded),
            "rmi" => ok(""),
            _ => {
                let entry = args.iter().position(|a| *a == "--entrypoint");
                let program = entry.map(|i| args[i + 1]);
                let rest: Vec<&str> = entry.map_or(vec![], |i| args[i + 3..].to_vec());
                match (program, rest.as_slice()) {
                    (None, _) if self.bare_ok => ok(""),
                    (None, _) => failed,
                    (Some("/usr/bin/id"), _) => ok(&self.uid),
                    (Some(_), ["--version", "--json"]) => ok(&self.json),
                    (Some(_), ["--version"]) => ok(&self.version),
                    other => panic!("not modelled: {other:?}"),
                }
            }
        }
    }
}

fn run_model(
    image: Image,
    model: &Model,
    commit: &str,
) -> (Result<Vec<String>, Error>, Vec<String>) {
    let mut calls = Vec::new();
    let archive = PathBuf::from("image.tar");
    let result = {
        let mut docker = |args: &[&str]| -> Result<Out, Error> {
            calls.push(args.join(" "));
            Ok(model.answer(args))
        };
        run(
            &mut docker,
            &Test {
                image,
                archive: &archive,
                commit,
            },
        )
    };
    (result, calls)
}

#[test]
fn a_good_frontend_image_loads_names_the_commit_and_is_removed() {
    let (result, calls) = run_model(Image::Frontend, &Model::good(Image::Frontend), COMMIT);
    let printed = result.expect("good");
    assert!(
        printed
            .iter()
            .any(|l| l.contains("--version --json") || l.contains("profile release")),
        "{printed:?}"
    );
    assert!(
        printed.iter().any(|l| l.contains("user id 10001")),
        "{printed:?}"
    );
    assert!(
        printed.iter().any(|l| l.contains("non-zero")),
        "{printed:?}"
    );
    assert_eq!(
        calls.first().map(String::as_str),
        Some("load --input image.tar")
    );
    assert_eq!(
        calls.last().map(String::as_str),
        Some(format!("rmi --force kbf-frontend:{COMMIT}").as_str())
    );
    assert!(
        calls
            .iter()
            .all(|c| !c.starts_with("run") || c.contains("--network none")),
        "{calls:?}"
    );
}

#[test]
fn the_coordinator_is_run_without_the_json_identity() {
    let mut model = Model::good(Image::Coordinator);
    model.json = "not asked".to_owned();
    let (result, calls) = run_model(Image::Coordinator, &model, COMMIT);
    result.expect("good");
    assert!(calls.iter().all(|c| !c.contains("--json")), "{calls:?}");
    assert!(
        calls
            .iter()
            .any(|c| c.contains("--entrypoint /usr/bin/kbf-coordinator")),
        "{calls:?}"
    );
}

#[test]
fn an_image_that_does_not_name_the_commit_or_run_as_the_user_is_refused() {
    let case = |image: Image, change: fn(&mut Model), want: &str| {
        let mut model = Model::good(image);
        change(&mut model);
        let (result, calls) = run_model(image, &model, COMMIT);
        let err = result.unwrap_err().to_string();
        assert!(err.contains(want), "{want}: {err}");
        // Whatever failed, the image is removed.
        assert!(
            calls.last().is_some_and(|c| c.starts_with("rmi")),
            "{calls:?}"
        );
    };
    case(
        Image::Frontend,
        |m| m.loaded = "Loaded image: kbf-frontend:latest\n".to_owned(),
        "did not load",
    );
    case(
        Image::Frontend,
        |m| m.version = format!("kbf 0.1.0+{}\n", "f".repeat(12)),
        "--version is",
    );
    case(
        Image::Coordinator,
        |m| m.version = "kbf-coordinator 0.1.0\n".to_owned(),
        "--version is",
    );
    case(
        Image::Frontend,
        |m| m.json = format!("{{\"commit\":\"{SHORT}\",\"profile\":\"debug\"}}"),
        "profile",
    );
    case(Image::Frontend, |m| m.uid = "0\n".to_owned(), "not 10001");
    case(
        Image::Frontend,
        |m| m.bare_ok = true,
        "exited 0 with nothing mounted",
    );
}

#[test]
fn a_docker_failure_is_reported_with_what_docker_said() {
    for step in ["load", "rmi"] {
        let mut model = Model::good(Image::Frontend);
        model.fail = Some(step);
        let (result, _) = run_model(Image::Frontend, &model, COMMIT);
        // The removal is best effort; a failed load is the verb's failure.
        if step == "load" {
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("docker load --input image.tar") && err.contains("boom"),
                "{err}"
            );
        } else {
            result.expect("a failed removal does not fail the checks");
        }
    }
    let (result, calls) = run_model(Image::Frontend, &Model::good(Image::Frontend), "abc");
    assert!(matches!(result.unwrap_err(), Error::NotACommit(_)));
    assert!(calls.is_empty(), "{calls:?}");
}

#[test]
fn a_program_that_cannot_run_is_the_spawn_error_of_the_closure() {
    let archive = PathBuf::from("image.tar");
    let mut docker =
        |_: &[&str]| -> Result<Out, Error> { Err(Error::Failed("no docker".to_owned())) };
    let err = run(
        &mut docker,
        &Test {
            image: Image::Frontend,
            archive: &archive,
            commit: COMMIT,
        },
    )
    .unwrap_err();
    assert_eq!(err, Error::Failed("no docker".to_owned()));
}

/// A directory holding a Dockerfile, for the build of an image.
fn dockerfile_dir(dir: &std::path::Path) -> PathBuf {
    let d = dir.join("df");
    std::fs::create_dir_all(&d).expect("dir");
    std::fs::write(d.join("Dockerfile"), "FROM scratch\n").expect("Dockerfile");
    d
}

/// `image-build` against a docker that answers `fail` for its first word with a failure.
fn build_with(
    name: &str,
    image: Image,
    commit: &str,
    fail: Option<&str>,
) -> (Result<Vec<String>, Error>, Vec<String>, PathBuf) {
    let dir = scratch(name);
    let df = dockerfile_dir(&dir);
    let binary = binary_file(&dir);
    let out = dir.join("work");
    let mut calls = Vec::new();
    let result = {
        let mut docker = |args: &[&str]| -> Result<Out, Error> {
            calls.push(args.join(" "));
            Ok(Out {
                ok: fail != Some(args[0]),
                stdout: String::new(),
                stderr: "boom".to_owned(),
            })
        };
        build(
            &mut docker,
            &Build {
                image,
                dockerfile_dir: &df,
                binary: &binary,
                commit,
                out: &out,
            },
        )
    };
    (result, calls, out)
}

#[test]
fn the_context_holds_the_dockerfile_and_the_binary_and_the_archive_is_saved_and_unloaded() {
    let (result, calls, out) = build_with("build", Image::Coordinator, COMMIT, None);
    result.expect("built");
    let context = out.join("kbf-coordinator-context");
    let mut files: Vec<String> = std::fs::read_dir(&context)
        .expect("context")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files, ["Dockerfile", "kbf-coordinator"]);
    assert_eq!(
        std::fs::read(context.join("kbf-coordinator")).expect("binary"),
        BINARY
    );
    let tag = format!("kbf-coordinator:{COMMIT}");
    assert_eq!(
        calls,
        [
            format!(
                "build --build-arg COMMIT={COMMIT} --tag {tag} {}",
                context.display()
            ),
            format!(
                "save --output {} {tag}",
                out.join("kbf-coordinator.tar").display()
            ),
            format!("rmi --force {tag}"),
        ]
    );
}

#[test]
fn a_failed_build_or_save_is_reported_and_a_saved_image_is_still_unloaded() {
    let (result, calls, _) = build_with("build-fails", Image::Frontend, COMMIT, Some("build"));
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("docker build") && err.contains("boom"),
        "{err}"
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    let (result, calls, _) = build_with("save-fails", Image::Frontend, COMMIT, Some("save"));
    assert!(result.unwrap_err().to_string().contains("docker save"));
    assert!(
        calls.last().is_some_and(|c| c.starts_with("rmi --force")),
        "{calls:?}"
    );
}

#[test]
fn the_build_refuses_a_bad_commit_a_missing_input_and_a_context_that_exists() {
    let (result, calls, _) = build_with("bad-commit", Image::Frontend, "zzz", None);
    assert!(matches!(result.unwrap_err(), Error::NotACommit(_)));
    assert!(calls.is_empty());
    let dir = scratch("build-inputs");
    let df = dockerfile_dir(&dir);
    let binary = binary_file(&dir);
    let out = dir.join("work");
    let mut docker = |_: &[&str]| -> Result<Out, Error> { panic!("docker was run") };
    let missing_dockerfile = (dir.join("nowhere"), binary.clone(), out.clone());
    let missing_binary = (df.clone(), dir.join("absent"), dir.join("work2"));
    for (what, (d, b, o)) in [
        ("Dockerfile", missing_dockerfile),
        ("absent", missing_binary),
    ] {
        let err = build(
            &mut docker,
            &Build {
                image: Image::Frontend,
                dockerfile_dir: &d,
                binary: &b,
                commit: COMMIT,
                out: &o,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains(what), "{what}: {err}");
    }
    // The first attempt left its context behind; a second one in the same directory does not
    // build into it.
    let err = build(
        &mut docker,
        &Build {
            image: Image::Frontend,
            dockerfile_dir: &df,
            binary: &binary,
            commit: COMMIT,
            out: &out,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("kbf-frontend-context"), "{err}");
}

#[test]
fn an_archive_or_layer_cut_short_is_a_problem() {
    let dir = scratch("short");
    let binary = binary_file(&dir);
    // The outer archive, cut inside its first file.
    let whole = Fix::good().archive();
    let found = problems(&whole[..700], &check(&binary)).expect("checked");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(said(&found, "not a tar"), "{found:?}");
    // A layer, cut inside the binary.
    let mut fix = Fix::good();
    let big = layer(&[file("usr/bin/kbf", 0o755, &[1u8; 4000])]);
    fix.layers[1] = big[..700].to_vec();
    let found = problems(&fix.archive(), &check(&binary)).expect("checked");
    assert!(said(&found, "a layer is not a tar"), "{found:?}");
}

/// A reader that fails.
struct Failing;

impl std::io::Read for Failing {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk gone"))
    }
}

#[test]
fn an_entry_that_cannot_be_read_names_its_path() {
    assert_eq!(
        body(&mut Failing, "usr/bin/kbf"),
        Err("usr/bin/kbf: disk gone".to_owned())
    );
    assert_eq!(body(&mut &b"abc"[..], "x"), Ok(b"abc".to_vec()));
}
