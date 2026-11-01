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
    layout_edited(config, |_| {})
}

/// `layout_with`, with `change` applied to the manifest before it is stored and named by the index.
fn layout_edited(config: &serde_json::Value, change: impl FnOnce(&mut serde_json::Value)) -> Files {
    let layer = b"the layer's bytes".to_vec();
    let config = serde_json::to_vec(config).expect("config");
    let mut manifest = serde_json::json!({
        "config": descriptor(&config),
        "layers": [descriptor(&layer)],
    });
    change(&mut manifest);
    let manifest = serde_json::to_vec(&manifest).expect("manifest");
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

#[test]
fn the_tree_is_a_new_directory_holding_the_binary_alone_mode_0755() {
    // Asserts: `<out>/kbf-task` has the binary's bytes and mode 0755 and is the only entry; an existing
    // directory is refused; a binary not named kbf-task is refused, and nothing is made for it.
    let dir = scratch("tree");
    let bin = dir.join("target/kbf-task");
    std::fs::create_dir_all(bin.parent().expect("parent")).expect("dirs");
    std::fs::write(&bin, b"signed bytes").expect("binary");
    let out = dir.join("tmp/task-tree");
    stage_tree(&bin, &out).expect("stages");
    assert_eq!(
        std::fs::read(out.join("kbf-task")).expect("file"),
        b"signed bytes"
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(out.join("kbf-task"))
            .expect("meta")
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(std::fs::read_dir(&out).expect("list").count(), 1);
    let again = stage_tree(&bin, &out).expect_err("existing directory");
    assert!(again.to_string().contains("create"), "{again}");
    let other = dir.join("target/kbf-other");
    std::fs::write(&other, b"x").expect("other");
    let wrong = stage_tree(&other, &dir.join("tmp/other-tree")).expect_err("name");
    assert!(
        wrong.to_string().contains("is not a file named kbf-task"),
        "{wrong}"
    );
    assert!(!dir.join("tmp/other-tree").exists());
}

/// A gzip of `tar`.
fn gzip(tar: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), Compression::fast());
    std::io::Write::write_all(&mut gz, tar).expect("gzip");
    gz.finish().expect("gzip")
}

/// The gzip tar of `entries` (header and body each), as a hand-made tarball of a layout.
fn tgz_of(entries: &[(tar::Header, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (h, body) in entries {
        b.append(h, *body).expect("append");
    }
    gzip(&b.into_inner().expect("tar"))
}

#[test]
fn a_layout_with_a_broken_oci_layout_file_or_index_is_refused_by_name() {
    // Asserts: an oci-layout that is not JSON, one that names another version, and an index
    // that is missing or lists no manifest are each named, and a missing index is named once (not
    // also as a count of manifests).
    let mut broken = good();
    broken.insert("oci-layout".to_owned(), b"not json".to_vec());
    only(&check_layout(&broken), "oci-layout is not JSON");

    let mut version = good();
    version.insert(
        "oci-layout".to_owned(),
        br#"{"imageLayoutVersion":"2.0.0"}"#.to_vec(),
    );
    only(
        &check_layout(&version),
        "oci-layout: imageLayoutVersion is not 1.0.0",
    );

    let mut no_index = good();
    no_index.remove("index.json");
    let found = check_layout(&no_index);
    assert!(
        found.contains(&"index.json is missing".to_owned()),
        "{found:?}"
    );
    assert!(!found.iter().any(|p| p.contains("manifests")), "{found:?}");

    let mut none = good();
    none.insert("index.json".to_owned(), br#"{"manifests":[]}"#.to_vec());
    let found = check_layout(&none);
    assert!(
        found
            .iter()
            .any(|p| p.contains("lists 0 manifests, not exactly one")),
        "{found:?}"
    );
}

#[test]
fn a_descriptor_that_does_not_fit_its_blob_is_refused_by_name() {
    // Asserts: a digest that is not 64 lowercase hex digits after `sha256:` (too short, capitals,
    // another algorithm), a blob the descriptor names that is absent, and a size that is not the
    // blob's length are each named with the descriptor they come from; a manifest with two layers
    // or none is refused by its count.
    let capitals = format!("sha256:{}", "A".repeat(64));
    for digest in ["sha256:abc", capitals.as_str(), "md5:xyz"] {
        let files = layout_edited(&good_config(), |m| m["config"]["digest"] = digest.into());
        let found = check_layout(&files);
        assert!(
            found.iter().any(|p| p.contains(&format!(
                "the manifest's config: {digest:?} is not a sha256 digest"
            ))),
            "{digest}: {found:?}"
        );
    }

    let mut gone = good();
    let layer = gone
        .keys()
        .find(|p| gone[*p] == b"the layer's bytes")
        .cloned()
        .expect("layer");
    gone.remove(&layer);
    only(
        &check_layout(&gone),
        &format!("the manifest's layer: its blob {layer} is missing"),
    );

    let short = layout_edited(&good_config(), |m| m["layers"][0]["size"] = 1.into());
    only(&check_layout(&short), "is not 1 bytes");

    let two = layout_edited(&good_config(), |m| {
        let layer = m["layers"][0].clone();
        m["layers"] = serde_json::json!([layer.clone(), layer]);
    });
    let found = check_layout(&two);
    assert!(
        found
            .iter()
            .any(|p| p.contains("the manifest lists 2 layers, not one")),
        "{found:?}"
    );
    let zero = layout_edited(&good_config(), |m| m["layers"] = serde_json::json!([]));
    let found = check_layout(&zero);
    assert!(
        found
            .iter()
            .any(|p| p.contains("the manifest lists 0 layers, not one")),
        "{found:?}"
    );
}

#[test]
fn a_path_too_long_for_a_tar_header_is_refused_by_the_tarball_writer() {
    // Asserts: a file name of 300 characters is an error naming the path, not a truncated entry.
    let mut files = good();
    let long = "a".repeat(300);
    files.insert(long.clone(), Vec::new());
    let err = tarball(&files).expect_err("too long");
    assert!(
        err.contains("cannot be a tar entry name") && err.contains(&long),
        "{err}"
    );
}

#[test]
fn a_tarball_that_is_not_a_gzip_tar_is_named() {
    // Asserts: bytes that are not gzip, and a gzip of bytes that are not a tar, are each one problem.
    let files = good();
    let found = check_tarball(b"plain text, not gzip", &files);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("not a gzip stream: "), "{found:?}");

    let found = check_tarball(&gzip(&[b'A'; 1024]), &files);
    assert!(
        found.iter().any(|p| p.starts_with("not a tar: ")),
        "{found:?}"
    );
}

#[test]
fn a_tarball_entry_of_another_type_with_an_owner_name_or_listed_twice_is_named() {
    // Asserts: a symlink entry is refused by type (and its fields not examined), an entry with an
    // owner name is refused, and a path listed twice is named; the other files of the layout are
    // then reported missing, one problem per path.
    let files = good();
    let link = header("index.json", tar::EntryType::Symlink, 0, FILE_MODE).expect("header");
    let found = check_tarball(&tgz_of(&[(link, &b""[..])]), &files);
    assert!(
        found
            .iter()
            .any(|p| p.starts_with("index.json: the type is Symlink, not a file or a directory")),
        "{found:?}"
    );
    assert!(
        !found.iter().any(|p| p.contains("the mode")),
        "a refused type is not also examined: {found:?}"
    );

    let mut named = header("index.json", tar::EntryType::Regular, 0, FILE_MODE).expect("header");
    named.set_username("root").expect("name");
    named.set_cksum();
    let found = check_tarball(&tgz_of(&[(named, &b""[..])]), &files);
    assert!(
        found.contains(&"index.json: the owner name is not empty".to_owned()),
        "{found:?}"
    );

    let once = header("index.json", tar::EntryType::Regular, 0, FILE_MODE).expect("header");
    let found = check_tarball(
        &tgz_of(&[(once.clone(), &b""[..]), (once, &b""[..])]),
        &files,
    );
    assert!(
        found.contains(&"index.json: listed twice".to_owned()),
        "{found:?}"
    );
}

#[test]
fn a_directory_entry_with_the_wrong_mode_is_named() {
    // Asserts: a directory the tar lists is held to mode 0755 like a file to 0644, so a tar of
    // the right paths with another mode is refused.
    let files = good();
    let dir = header("blobs/", tar::EntryType::Directory, 0, 0o700).expect("header");
    let found = check_tarball(&tgz_of(&[(dir, b"")]), &files);
    assert!(
        found
            .iter()
            .any(|p| p.contains("blobs/: the mode is Some(448), not 493")),
        "{found:?}"
    );
}

#[test]
fn a_symlink_in_the_layout_directory_and_an_empty_sbom_are_refused_and_nothing_is_written() {
    // Asserts: a symlink in the layout directory is named as neither a directory nor a regular
    // file (it is not followed), and the verb writes no asset for it; an empty SBOM is refused
    // by name, also before anything is written.
    let dir = scratch("odd");
    let layout = dir.join("layout");
    write_layout(&layout, &good());
    std::os::unix::fs::symlink("index.json", layout.join("link")).expect("symlink");
    let sbom = dir.join("sbom.cdx.json");
    std::fs::write(&sbom, b"{}").expect("sbom");
    let out = dir.join("dist");
    let err = package_task_bundle(&layout, &sbom, COMMIT, &out).expect_err("symlink");
    assert!(
        err.to_string()
            .contains("link: neither a directory nor a regular file"),
        "{err}"
    );
    assert!(!out.exists());

    let clean = dir.join("clean");
    write_layout(&clean, &good());
    let empty = dir.join("empty.cdx.json");
    std::fs::write(&empty, b"").expect("empty");
    let err = package_task_bundle(&clean, &empty, COMMIT, &out).expect_err("empty sbom");
    assert!(err.to_string().contains("is empty"), "{err}");
    assert!(!out.exists());
}

#[test]
fn a_tarball_that_reads_back_as_another_layout_is_refused_and_no_sbom_or_sum_is_written() {
    // Asserts: when the file written is not the layout's tarball (here the writer is handed
    // another layout's), the verb names the tarball's path and the differing file, and leaves no
    // SBOM and no SHA256SUMS line behind.
    let dir = scratch("readback");
    let layout = dir.join("layout");
    write_layout(&layout, &good());
    let sbom = dir.join("sbom.cdx.json");
    std::fs::write(&sbom, b"{}").expect("sbom");
    let out = dir.join("dist");
    let mut other = good();
    other.insert("extra".to_owned(), b"x".to_vec());
    let wrong = |_: &Files| tarball(&other);
    let err = package_with(&layout, &sbom, COMMIT, &out, &wrong).expect_err("readback");
    let text = err.to_string();
    assert!(
        text.contains(&format!(
            "kbf-task-bundle-{COMMIT}-darwin-arm64.tar.gz: extra: not a file of the layout"
        )),
        "{text}"
    );
    assert!(
        !out.join(format!("kbf-task-bundle-{COMMIT}-darwin-arm64.cdx.json"))
            .exists()
            && !out.join("SHA256SUMS.darwin-arm64").exists()
    );
}

#[test]
fn a_tarball_writer_that_fails_stops_the_verb() {
    // Asserts: the writer's own error reaches the caller unchanged and nothing is written.
    let dir = scratch("writer");
    let layout = dir.join("layout");
    write_layout(&layout, &good());
    let sbom = dir.join("sbom.cdx.json");
    std::fs::write(&sbom, b"{}").expect("sbom");
    let out = dir.join("dist");
    let fails = |_: &Files| Err("no tar for you".to_owned());
    let err = package_with(&layout, &sbom, COMMIT, &out, &fails).expect_err("writer");
    assert!(err.to_string().contains("no tar for you"), "{err}");
    assert!(!out.join("SHA256SUMS.darwin-arm64").exists());
}

#[test]
fn a_tree_whose_directory_cannot_be_made_is_refused_by_name() {
    // Asserts: an --out below a regular file, and an --out with no parent (the root), are
    // errors naming what could not be created, and the binary is not copied.
    let dir = scratch("tree-errors");
    let bin = dir.join("kbf-task");
    std::fs::write(&bin, b"signed bytes").expect("binary");
    let file = dir.join("a-file");
    std::fs::write(&file, b"x").expect("file");
    let err = stage_tree(&bin, &file.join("below/tree")).expect_err("parent is a file");
    assert!(
        err.to_string()
            .contains(&format!("create {}", file.join("below").display())),
        "{err}"
    );
    let err = stage_tree(&bin, Path::new("/")).expect_err("the root");
    assert!(err.to_string().contains("create /"), "{err}");
}
