//! `package-task-bundle --layout <dir> --sbom <path> --commit <sha> --out <dir>`: the signed darwin
//! `kbf-task` as its own release asset, a jobscheduler bundle (docs/artifacts.md, "The macOS
//! `kbf-task` bundle").
//!
//! The darwin job packs the signed `kbf-task` with the pinned `js-bundle pack` into an OCI image
//! layout directory (`oci-layout`, `index.json`, `blobs/sha256/<hex>`). This verb refuses a
//! directory that is not exactly one such bundle of `kbf-task` ([`check_layout`]), then writes, in
//! `<out>`:
//!
//! - `kbf-task-bundle-<commit>-darwin-arm64.tar.gz`: the layout directory's files, at the
//!   paths they have in it (so it extracts to a layout directory), the same bytes on every runner:
//!   entries sorted by path, mode 0644 (0755 for a directory), owner and group 0 and no owner names,
//!   time 0, in a gzip stream with no file name and time 0 ([`tarball`]);
//! - `kbf-task-bundle-<commit>-darwin-arm64.cdx.json`: a copy of `--sbom` (the SBOM of the `kbf-task`
//!   binary, which is all the one layer holds);
//!
//! and appends both files' lines to `<out>/SHA256SUMS.darwin-arm64` ([`crate::sums`]). Neither
//! asset may exist already, and the sums file may not list either. After writing the tarball it reads
//! the file back and refuses it unless it holds exactly the layout's files ([`check_tarball`]).
//!
//! The layer's bytes are not read here: the job that runs this verb has packed the binary it signed
//! (the workflow lint holds the order of the steps), and runs `js-bundle verify` on the layout, which
//! extracts every layer under the format's rules.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use flate2::Compression;

use crate::fetch::Sha256;
use crate::package::{Platform, asset_stem, open_sums, read, write_new, write_sums};
use crate::{Error, build_commit, sums};

/// The name the asset has in place of a binary's: `kbf-task-bundle-<commit>-darwin-arm64`.
pub const NAME: &str = "kbf-task-bundle";

/// The platform of the bundle: a Mac runs it.
pub const PLATFORM: Platform = Platform::DarwinArm64;

/// The name the bundle has in its own config (`js-bundle pack --name`), and the one program it names
/// (`--entrypoint kbf-task=...`).
pub const BUNDLE_NAME: &str = "kbf-task";

/// The mode of a file in the tarball, and of a directory.
const FILE_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;

/// The files of a layout directory: the path below it, `/`-separated, and the bytes.
pub type Files = BTreeMap<String, Vec<u8>>;

/// The files below `dir`; a path that is neither a directory nor a regular file is a problem.
fn walk(
    dir: &Path,
    prefix: &str,
    files: &mut Files,
    problems: &mut Vec<String>,
) -> Result<(), Error> {
    let entries =
        std::fs::read_dir(dir).map_err(Error::spawn(format!("list {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::spawn(format!("list {}", dir.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{prefix}{name}");
        let kind = entry
            .file_type()
            .map_err(Error::spawn(format!("stat {}", entry.path().display())))?;
        if kind.is_dir() {
            walk(&entry.path(), &format!("{path}/"), files, problems)?;
        } else if kind.is_file() {
            files.insert(path, read(&entry.path())?);
        } else {
            problems.push(format!("{path}: neither a directory nor a regular file"));
        }
    }
    Ok(())
}

/// The digest `sha256:<hex>` of a blob's bytes.
fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256::of(bytes))
}

/// The blob path of a digest `sha256:<hex>`, when it is one.
fn blob_path(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| format!("blobs/sha256/{hex}"))
}

/// The JSON of `files[path]`, or a problem.
fn json(files: &Files, path: &str, problems: &mut Vec<String>) -> Option<serde_json::Value> {
    let Some(bytes) = files.get(path) else {
        problems.push(format!("{path} is missing"));
        return None;
    };
    match serde_json::from_slice(bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            problems.push(format!("{path} is not JSON: {e}"));
            None
        }
    }
}

/// What is wrong with `files` as the OCI image layout of one `kbf-task` bundle: one line per
/// problem, none when it is. It holds exactly `oci-layout`, `index.json` and the blobs, no other
/// file; `index.json` names one manifest; the manifest names a config and one layer; every blob is
/// named by one of them, is stored under the SHA-256 of its bytes and is as long as its
/// descriptor says; and the config says a darwin arm64 bundle named [`BUNDLE_NAME`] with one
/// layer and the entrypoint [`BUNDLE_NAME`]. (The manifest and config shapes are `js-bundle`'s to
/// validate in full: the job runs `js-bundle verify` as well.)
pub fn check_layout(files: &Files) -> Vec<String> {
    let mut problems = Vec::new();
    for path in files.keys() {
        if path != "oci-layout" && path != "index.json" && !path.starts_with("blobs/sha256/") {
            problems.push(format!("{path}: not a file of an OCI image layout"));
        }
    }
    if let Some(layout) = json(files, "oci-layout", &mut problems)
        && layout["imageLayoutVersion"] != "1.0.0"
    {
        problems.push("oci-layout: imageLayoutVersion is not 1.0.0".to_owned());
    }
    let mut named: Vec<(String, String)> = Vec::new();
    let mut describe = |what: &str, d: &serde_json::Value, problems: &mut Vec<String>| {
        let digest = d["digest"].as_str().unwrap_or_default();
        let Some(path) = blob_path(digest) else {
            problems.push(format!("{what}: {digest:?} is not a sha256 digest"));
            return None;
        };
        match files.get(&path) {
            None => problems.push(format!("{what}: its blob {path} is missing")),
            Some(bytes) => {
                if d["size"].as_u64() != Some(bytes.len() as u64) {
                    problems.push(format!("{what}: {path} is not {} bytes", d["size"]));
                }
            }
        }
        named.push((what.to_owned(), path.clone()));
        Some(path)
    };
    let index = json(files, "index.json", &mut problems);
    let manifests = index
        .as_ref()
        .and_then(|i| i["manifests"].as_array().cloned())
        .unwrap_or_default();
    let manifest_path = match manifests.as_slice() {
        [one] => describe("index.json manifest", one, &mut problems),
        _ => {
            if index.is_some() {
                problems.push(format!(
                    "index.json lists {} manifests, not exactly one",
                    manifests.len()
                ));
            }
            None
        }
    };
    let manifest = manifest_path.and_then(|p| json(files, &p, &mut problems));
    let mut config_path = None;
    if let Some(manifest) = &manifest {
        config_path = describe("the manifest's config", &manifest["config"], &mut problems);
        let layers = manifest["layers"].as_array().cloned().unwrap_or_default();
        if layers.len() == 1 {
            describe("the manifest's layer", &layers[0], &mut problems);
        } else {
            problems.push(format!(
                "the manifest lists {} layers, not one",
                layers.len()
            ));
        }
    }
    if let Some(config) = config_path.and_then(|p| json(files, &p, &mut problems)) {
        let says = |field: &str, want: &str| {
            if config[field] != want {
                Some(format!(
                    "the config's {field} is {}, not {want:?}",
                    config[field]
                ))
            } else {
                None
            }
        };
        problems.extend(says("os", "darwin"));
        problems.extend(says("architecture", "arm64"));
        problems.extend(says("name", BUNDLE_NAME));
        if config["layers"].as_array().map(Vec::len) != Some(1) {
            problems.push("the config lists no single layer".to_owned());
        }
        if config["entrypoints"].get(BUNDLE_NAME).is_none() {
            problems.push(format!("the config has no entrypoint {BUNDLE_NAME}"));
        }
    }
    // Every blob is one the manifest names, and stored under its own digest.
    for (path, bytes) in files.iter().filter(|(p, _)| p.starts_with("blobs/sha256/")) {
        if !named.iter().any(|(_, n)| n == path) {
            problems.push(format!("{path}: no descriptor names it"));
        }
        if blob_path(&digest_of(bytes)).as_deref() != Some(path) {
            problems.push(format!(
                "{path}: is not stored under the SHA-256 of its bytes"
            ));
        }
    }
    problems
}

/// The directories of `files` (`blobs`, `blobs/sha256`), in the order the tar lists them.
fn directories(files: &Files) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    for path in files.keys() {
        let parts: Vec<&str> = path.split('/').collect();
        for n in 1..parts.len() {
            let dir = parts[..n].join("/");
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs.sort();
    dirs
}

/// The header every entry has: owner and group 0, no names, time 0.
fn header(path: &str, kind: tar::EntryType, size: u64, mode: u32) -> Result<tar::Header, String> {
    let mut h = tar::Header::new_ustar();
    h.set_path(path)
        .map_err(|e| format!("{path:?} cannot be a tar entry name: {e}"))?;
    h.set_entry_type(kind);
    h.set_size(size);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    Ok(h)
}

/// The gzip-compressed tar of `files`: every directory, then every file, each sorted by path.
///
/// # Errors
/// A path does not fit a ustar header.
pub fn tarball(files: &Files) -> Result<Vec<u8>, String> {
    let gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    let mut tar = tar::Builder::new(gzip);
    for dir in directories(files) {
        let h = header(&format!("{dir}/"), tar::EntryType::Directory, 0, DIR_MODE)?;
        tar.append(&h, std::io::empty()).expect("tar into memory");
    }
    for (path, bytes) in files {
        let h = header(path, tar::EntryType::Regular, bytes.len() as u64, FILE_MODE)?;
        tar.append(&h, &bytes[..]).expect("tar into memory");
    }
    let gzip = tar.into_inner().expect("tar into memory");
    Ok(gzip.finish().expect("gzip into memory"))
}

/// What is wrong with `tgz` as the tarball of `files`: one line per problem, none when it is exactly
/// what [`tarball`] writes for them (compared entry by entry: type, path, mode, owner, group, time
/// and bytes).
pub fn check_tarball(tgz: &[u8], files: &Files) -> Vec<String> {
    let mut tar = Vec::new();
    if let Err(e) = flate2::read::GzDecoder::new(tgz).read_to_end(&mut tar) {
        return vec![format!("not a gzip stream: {e}")];
    }
    let mut problems = Vec::new();
    let mut seen: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut archive = tar::Archive::new(&tar[..]);
    let all = archive.entries().expect("a fresh archive");
    for entry in all {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                problems.push(format!("not a tar: {e}"));
                break;
            }
        };
        let h = entry.header().clone();
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let dir = h.entry_type() == tar::EntryType::Directory;
        let (kind, mode) = if dir {
            (tar::EntryType::Directory, DIR_MODE)
        } else {
            (tar::EntryType::Regular, FILE_MODE)
        };
        if h.entry_type() != kind {
            problems.push(format!(
                "{path}: the type is {:?}, not a file or a directory",
                h.entry_type()
            ));
            continue;
        }
        let fields = [
            ("the mode", h.mode().ok().map(u64::from), u64::from(mode)),
            ("the owner", h.uid().ok(), 0),
            ("the group", h.gid().ok(), 0),
            ("the time", h.mtime().ok(), 0),
        ];
        for (what, got, want) in fields {
            if got != Some(want) {
                problems.push(format!("{path}: {what} is {got:?}, not {want}"));
            }
        }
        if (h.username_bytes(), h.groupname_bytes()) != (Some(&[][..]), Some(&[][..])) {
            problems.push(format!("{path}: the owner name is not empty"));
        }
        let mut held = Vec::new();
        // A read of an entry of a tar held in memory cannot fail: a short body shows as other bytes.
        entry.read_to_end(&mut held).expect("a tar entry in memory");
        if seen.insert(path.clone(), held).is_some() {
            problems.push(format!("{path}: listed twice"));
        }
    }
    let wanted: Vec<String> = directories(files)
        .into_iter()
        .map(|d| format!("{d}/"))
        .chain(files.keys().cloned())
        .collect();
    for path in &wanted {
        match seen.get(path) {
            None => problems.push(format!("{path}: missing from the tarball")),
            Some(bytes) => {
                if let Some(want) = files.get(path)
                    && bytes != want
                {
                    problems.push(format!("{path}: not the layout's bytes"));
                }
            }
        }
    }
    for path in seen.keys().filter(|p| !wanted.contains(p)) {
        problems.push(format!("{path}: not a file of the layout"));
    }
    problems
}

/// The `stage-task-tree` verb: `binary` (a file named `kbf-task`) copied, mode 0755, to `<out>/kbf-task` in a
/// directory made new for it, the one tree `js-bundle pack` packs. The line to print.
pub fn stage_tree(binary: &Path, out: &Path) -> Result<String, Error> {
    if binary.file_name().and_then(|n| n.to_str()) != Some(BUNDLE_NAME) {
        return Err(Error::Failed(format!(
            "{} is not a file named {BUNDLE_NAME}",
            binary.display()
        )));
    }
    let bytes = read(binary)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(Error::spawn(format!("create {}", parent.display())))?;
    }
    std::fs::create_dir(out).map_err(Error::spawn(format!("create {}", out.display())))?;
    let file = out.join(BUNDLE_NAME);
    write_new(&file, &bytes)?;
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(Error::spawn(format!("chmod {}", file.display())))?;
    Ok(format!("staged {} as {}", binary.display(), file.display()))
}

/// The `package-task-bundle` verb: the lines to print.
pub fn package_task_bundle(
    layout: &Path,
    sbom: &Path,
    commit: &str,
    out: &Path,
) -> Result<Vec<String>, Error> {
    package_with(layout, sbom, commit, out, &tarball)
}

/// [`package_task_bundle`] with the tarball writer given, so a test can hand the read-back a
/// tarball that is not the layout's.
fn package_with(
    layout: &Path,
    sbom: &Path,
    commit: &str,
    out: &Path,
    pack: &dyn Fn(&Files) -> Result<Vec<u8>, String>,
) -> Result<Vec<String>, Error> {
    if !build_commit::is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    let mut files = Files::new();
    let mut problems = Vec::new();
    walk(layout, "", &mut files, &mut problems)?;
    problems.extend(check_layout(&files));
    if !problems.is_empty() {
        return Err(Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", layout.display()))
                .collect(),
        ));
    }
    let sbom_bytes = read(sbom)?;
    if sbom_bytes.is_empty() {
        return Err(Error::Failed(format!(
            "the SBOM {} is empty",
            sbom.display()
        )));
    }
    let stem = asset_stem(NAME, commit, PLATFORM);
    let tgz_name = format!("{stem}.tar.gz");
    let sbom_name = format!("{stem}.cdx.json");
    let (sums_path, mut lines) = open_sums(out, PLATFORM)?;
    if let Some(l) = lines
        .iter()
        .find(|l| [&tgz_name, &sbom_name].contains(&&l.name))
    {
        return Err(Error::Failed(format!(
            "{} already lists {}",
            sums_path.display(),
            l.name
        )));
    }
    let tgz = pack(&files).map_err(Error::Failed)?;
    let path = out.join(&tgz_name);
    write_new(&path, &tgz)?;
    let found = check_tarball(&read(&path)?, &files);
    if !found.is_empty() {
        return Err(Error::Refused(
            found
                .into_iter()
                .map(|p| format!("{}: {p}", path.display()))
                .collect(),
        ));
    }
    write_new(&out.join(&sbom_name), &sbom_bytes)?;
    lines.push(sums::Line {
        sha256: Sha256::of(&tgz),
        name: tgz_name,
    });
    lines.push(sums::Line {
        sha256: Sha256::of(&sbom_bytes),
        name: sbom_name,
    });
    let text = write_sums(&sums_path, &lines)?;
    let mut printed = vec![format!("packaged {stem}:")];
    printed.extend(text.lines().map(str::to_owned));
    Ok(printed)
}

#[cfg(test)]
mod tests;
