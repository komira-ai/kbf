use std::path::PathBuf;

use super::*;
use kbf_testkit::dir::TestDir;

const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-task-bundle-unit"), name)
}

fn descriptor(bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({"digest": digest_of(bytes), "size": bytes.len()})
}

/// A layout as `js-bundle pack` writes one for `kbf-task`: a config, one layer, a manifest, an index.
fn layout_with(config: &serde_json::Value) -> Files {
    let layer = b"the layer's bytes".to_vec();
    let config = serde_json::to_vec(config).expect("config");
    let manifest = serde_json::to_vec(&serde_json::json!({
        "config": descriptor(&config),
        "layers": [descriptor(&layer)],
    }))
    .expect("manifest");
    let index = serde_json::to_vec(&serde_json::json!({"manifests": [descriptor(&manifest)]}))
        .expect("index");
    let mut files = Files::new();
    files.insert(
        "oci-layout".to_owned(),
        br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
    );
    files.insert("index.json".to_owned(), index);
    for blob in [layer, config, manifest] {
        files.insert(blob_path(&digest_of(&blob)).expect("path"), blob);
    }
    files
}

fn good_config() -> serde_json::Value {
    serde_json::json!({
        "os": "darwin",
        "architecture": "arm64",
        "name": "kbf-task",
        "layers": [{"tree": "task"}],
        "entrypoints": {"kbf-task": {"path": "${tree:task}/kbf-task"}},
    })
}

fn good() -> Files {
    layout_with(&good_config())
}

/// `good()` with `change` applied to its config.
fn with_config(change: impl FnOnce(&mut serde_json::Value)) -> Files {
    let mut config = good_config();
    change(&mut config);
    layout_with(&config)
}

fn only(problems: &[String], text: &str) {
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains(text), "{problems:?}");
}

fn write_layout(dir: &Path, files: &Files) {
    for (path, bytes) in files {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        std::fs::write(file, bytes).expect("file");
    }
}

#[test]
fn a_layout_of_one_darwin_arm64_kbf_task_bundle_is_accepted() {
    // Asserts: the shape `js-bundle pack` writes passes, so every refusal below comes from the change made to it
    // and not from the fixture.
    assert_eq!(check_layout(&good()), Vec::<String>::new());
}

#[test]
fn a_layout_that_is_not_one_kbf_task_bundle_is_refused_by_name() {
    let mut extra = good();
    extra.insert("notes.txt".to_owned(), b"x".to_vec());
    only(
        &check_layout(&extra),
        "notes.txt: not a file of an OCI image layout",
    );

    only(
        &check_layout(&with_config(|c| c["os"] = "linux".into())),
        "the config's os is \"linux\", not \"darwin\"",
    );
    only(
        &check_layout(&with_config(|c| c["architecture"] = "amd64".into())),
        "the config's architecture",
    );
    only(
        &check_layout(&with_config(|c| c["name"] = "kbf".into())),
        "the config's name",
    );
    only(
        &check_layout(&with_config(|c| c["entrypoints"] = serde_json::json!({}))),
        "no entrypoint kbf-task",
    );
    only(
        &check_layout(&with_config(|c| c["layers"] = serde_json::json!([]))),
        "the config lists no single layer",
    );

    let mut stray = good();
    stray.insert("blobs/sha256/".to_owned() + &"0".repeat(64), Vec::new());
    let found = check_layout(&stray);
    assert!(
        found.iter().any(|p| p.contains("no descriptor names it")),
        "{found:?}"
    );

    // A blob under another blob's name: the digest in the path is not the digest of the bytes.
    let mut renamed = good();
    let (path, bytes) = renamed
        .iter()
        .find(|(p, _)| p.starts_with("blobs/"))
        .map(|(p, b)| (p.clone(), b.clone()))
        .expect("a blob");
    let mut flipped = bytes;
    flipped[0] ^= 1;
    renamed.insert(path, flipped);
    let found = check_layout(&renamed);
    assert!(
        found
            .iter()
            .any(|p| p.contains("not stored under the SHA-256 of its bytes")),
        "{found:?}"
    );

    let mut two = good();
    let index: serde_json::Value = serde_json::from_slice(&two["index.json"]).expect("index");
    let one = index["manifests"][0].clone();
    two.insert(
        "index.json".to_owned(),
        serde_json::to_vec(&serde_json::json!({"manifests": [one.clone(), one]})).expect("index"),
    );
    let found = check_layout(&two);
    assert!(
        found
            .iter()
            .any(|p| p.contains("lists 2 manifests, not exactly one")),
        "{found:?}"
    );

    let mut missing = good();
    missing.remove("oci-layout");
    only(&check_layout(&missing), "oci-layout is missing");
}

#[test]
fn the_tarball_is_the_layout_alone_owned_by_root_at_time_0_and_the_same_bytes_each_time() {
    // Asserts: two builds of one layout are byte for byte the same, and the read back finds
    // exactly the layout's directories and files, mode 0755 and 0644, owner 0, time 0.
    let files = good();
    let tgz = tarball(&files).expect("tarball");
    assert_eq!(tgz, tarball(&files).expect("again"));
    assert_eq!(check_tarball(&tgz, &files), Vec::<String>::new());
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(&tgz[..])
        .read_to_end(&mut tar)
        .expect("gunzip");
    let paths: Vec<String> = tar::Archive::new(&tar[..])
        .entries()
        .expect("entries")
        .map(|e| String::from_utf8_lossy(&e.expect("entry").path_bytes()).into_owned())
        .collect();
    assert_eq!(&paths[..2], ["blobs/", "blobs/sha256/"]);
    assert_eq!(paths.len(), 2 + files.len());
}

#[test]
fn a_tarball_that_differs_from_the_layout_is_named() {
    // Asserts: a tarball of another layout (a byte of a blob changed), one with a file the layout
    // lacks, one lacking a file, and one with a non-zero owner each fail check_tarball, by the path.
    let files = good();
    let mut changed = files.clone();
    let blob = changed
        .keys()
        .find(|p| p.starts_with("blobs/"))
        .cloned()
        .expect("blob");
    changed.get_mut(&blob).expect("blob")[0] ^= 1;
    let found = check_tarball(&tarball(&changed).expect("tarball"), &files);
    only(&found, &format!("{blob}: not the layout's bytes"));

    let mut more = files.clone();
    more.insert("extra".to_owned(), b"x".to_vec());
    only(
        &check_tarball(&tarball(&more).expect("tarball"), &files),
        "extra: not a file of the layout",
    );

    let mut fewer = files.clone();
    fewer.remove("index.json");
    only(
        &check_tarball(&tarball(&fewer).expect("tarball"), &files),
        "index.json: missing from the tarball",
    );

    let mut b = tar::Builder::new(Vec::new());
    let mut h = header("index.json", tar::EntryType::Regular, 0, FILE_MODE).expect("header");
    h.set_uid(1000);
    h.set_cksum();
    b.append(&h, std::io::empty()).expect("append");
    let tar = b.into_inner().expect("tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), Compression::fast());
    std::io::Write::write_all(&mut gz, &tar).expect("gzip");
    let found = check_tarball(&gz.finish().expect("gzip"), &files);
    assert!(
        found
            .iter()
            .any(|p| p.contains("index.json: the owner is Some(1000), not 0")),
        "{found:?}"
    );
}

#[test]
fn the_verb_writes_the_tarball_and_the_sbom_and_lists_both_once() {
    // Asserts: the two asset names, that the tarball read back is the layout, that the SBOM is a
    // copy, and that SHA256SUMS.darwin-arm64 has one line for each; a second run is refused and
    // writes nothing more; a bad layout is refused and leaves `out` without an asset.
    let dir = scratch("verb");
    let layout = dir.join("layout");
    let files = good();
    write_layout(&layout, &files);
    let sbom = dir.join("sbom.cdx.json");
    std::fs::write(&sbom, b"{\"bomFormat\":\"CycloneDX\"}").expect("sbom");
    let out: PathBuf = dir.join("dist");

    let printed = package_task_bundle(&layout, &sbom, COMMIT, &out).expect("package");
    assert_eq!(
        printed[0],
        format!("packaged kbf-task-bundle-{COMMIT}-darwin-arm64:")
    );
    let tgz_name = format!("kbf-task-bundle-{COMMIT}-darwin-arm64.tar.gz");
    let tgz = std::fs::read(out.join(&tgz_name)).expect("tarball");
    assert_eq!(check_tarball(&tgz, &files), Vec::<String>::new());
    assert_eq!(
        std::fs::read(out.join(format!("kbf-task-bundle-{COMMIT}-darwin-arm64.cdx.json")))
            .expect("sbom"),
        b"{\"bomFormat\":\"CycloneDX\"}"
    );
    let sums = std::fs::read_to_string(out.join("SHA256SUMS.darwin-arm64")).expect("sums");
    assert_eq!(sums.lines().count(), 2, "{sums}");
    assert!(
        sums.contains(&format!("{}  {tgz_name}", Sha256::of(&tgz))),
        "{sums}"
    );

    let again = package_task_bundle(&layout, &sbom, COMMIT, &out).expect_err("twice");
    assert!(again.to_string().contains("already lists"), "{again}");

    let bad = dir.join("bad");
    write_layout(&bad, &with_config(|c| c["os"] = "linux".into()));
    let other = dir.join("other");
    let refused = package_task_bundle(&bad, &sbom, COMMIT, &other).expect_err("bad layout");
    assert!(refused.to_string().contains("the config's os"), "{refused}");
    assert!(!other.exists(), "a refused layout must write nothing");
}

#[test]
fn a_commit_that_is_not_one_is_refused() {
    let dir = scratch("commit");
    let err = package_task_bundle(&dir, &dir, "main", &dir).expect_err("commit");
    assert!(matches!(err, Error::NotACommit(_)), "{err}");
}
