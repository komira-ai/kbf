//! What the tests of `base-tree` build with: layers as tars, an image of them as an OCI layout,
//! and a reader of the tree that tierstore's own flattener makes of the manifests.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use tierstore::{EntryKind, TreeDigest, TreeEntry, flatten};

use super::super::{Base, Tree};
use crate::fetch::Sha256;

/// One entry of a layer's tar.
pub struct E {
    path: Vec<u8>,
    kind: tar::EntryType,
    mode: u32,
    body: Vec<u8>,
    link: Vec<u8>,
    pax: Vec<(String, String)>,
    bad_mode: bool,
    raw_link: bool,
}

impl E {
    fn new(path: &[u8], kind: tar::EntryType) -> E {
        E {
            path: path.to_vec(),
            kind,
            mode: 0o644,
            body: Vec::new(),
            link: Vec::new(),
            pax: Vec::new(),
            bad_mode: false,
            raw_link: false,
        }
    }

    /// A regular file.
    pub fn file(path: &str, mode: u32, body: &[u8]) -> E {
        let mut e = E::new(path.as_bytes(), tar::EntryType::Regular);
        e.mode = mode;
        e.body = body.to_vec();
        e
    }

    /// A directory.
    pub fn dir(path: &str, mode: u32) -> E {
        let mut e = E::new(path.as_bytes(), tar::EntryType::Directory);
        e.mode = mode;
        e
    }

    /// A symlink to `target`.
    pub fn symlink(path: &str, target: &str) -> E {
        let mut e = E::new(path.as_bytes(), tar::EntryType::Symlink);
        e.link = target.as_bytes().to_vec();
        e
    }

    /// A hard link to the path `target` of the tar.
    pub fn hardlink(path: &str, mode: u32, target: &str) -> E {
        let mut e = E::new(path.as_bytes(), tar::EntryType::Link);
        e.mode = mode;
        e.link = target.as_bytes().to_vec();
        e
    }

    /// An entry of another type, with no body.
    pub fn special(path: &str, kind: tar::EntryType) -> E {
        E::new(path.as_bytes(), kind)
    }

    /// A whiteout: an empty regular file.
    pub fn whiteout(path: &str) -> E {
        E::file(path, 0, b"")
    }

    /// The same entry under a path of raw bytes.
    pub fn at(mut self, path: &[u8]) -> E {
        self.path = path.to_vec();
        self
    }

    /// The same entry with a link target of raw bytes.
    pub fn linking(mut self, target: &[u8]) -> E {
        self.link = target.to_vec();
        self
    }

    /// The same entry with its link target in the header byte for byte, as an image's tar may
    /// have it (`set_link_name` drops a `.` component and a trailing `/`).
    pub fn raw(mut self) -> E {
        self.raw_link = true;
        self
    }

    /// The same entry with a mode field that is not a number.
    pub fn garbled(mut self) -> E {
        self.bad_mode = true;
        self
    }

    /// The same entry preceded by a PAX record.
    pub fn pax(mut self, key: &str, value: &str) -> E {
        self.pax.push((key.to_owned(), value.to_owned()));
        self
    }
}

/// The PAX records `pax` as the body of an extended header.
fn pax_body(pax: &[(String, String)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (key, value) in pax {
        let rest = format!(" {key}={value}\n");
        let mut len = rest.len();
        loop {
            let total = rest.len() + len.to_string().len();
            if total == len {
                break;
            }
            len = total;
        }
        body.extend_from_slice(format!("{len}{rest}").as_bytes());
    }
    body
}

/// A header of `kind`, `size` bytes, named `path`.
fn header(path: &[u8], kind: tar::EntryType, mode: u32, size: usize) -> tar::Header {
    let mut h = tar::Header::new_ustar();
    // The name field as it is: `set_path` refuses an absolute path or one with `..`.
    h.as_old_mut().name[..path.len()].copy_from_slice(path);
    h.set_entry_type(kind);
    h.set_size(size as u64);
    h.set_mode(mode);
    h
}

/// The layer of `entries`, as a plain tar.
pub fn layer(entries: &[E]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for e in entries {
        if !e.pax.is_empty() {
            let body = pax_body(&e.pax);
            let mut h = header(b"PaxHeader/x", tar::EntryType::XHeader, 0o644, body.len());
            h.set_cksum();
            tar.append(&h, body.as_slice()).expect("pax");
        }
        let mut h = header(&e.path, e.kind, e.mode, e.body.len());
        if e.raw_link {
            h.as_old_mut().linkname[..e.link.len()].copy_from_slice(&e.link);
        } else if !e.link.is_empty() {
            h.set_link_name(OsStr::from_bytes(&e.link)).expect("link");
        }
        if e.bad_mode {
            h.as_old_mut().mode = *b"zzzzzzz\0";
        }
        h.set_cksum();
        tar.append(&h, e.body.as_slice()).expect("entry");
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

/// A plain tar of regular files `(path, bytes)`.
fn files_tar(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut h = header(path.as_bytes(), tar::EntryType::Regular, 0o644, bytes.len());
        h.set_cksum();
        tar.append(&h, bytes.as_slice()).expect("append");
    }
    tar.into_inner().expect("tar")
}

/// An image, as an OCI layout in a tar, and the base whose layers it begins with.
pub struct Img {
    pub bytes: Vec<u8>,
    pub base: Base,
}

/// The image of `base` layers and then `top` layers, with its layers gzip-compressed or plain.
pub fn image(base: &[Vec<u8>], top: &[Vec<u8>], gz: bool) -> Img {
    let pack = |l: &Vec<u8>| if gz { gzip(l) } else { l.clone() };
    let layers: Vec<Vec<u8>> = base.iter().chain(top).map(pack).collect();
    let digest = |b: &[u8]| format!("sha256:{}", Sha256::of(b));
    let config = br#"{"os":"linux","architecture":"amd64"}"#.to_vec();
    let listed: Vec<serde_json::Value> = layers
        .iter()
        .map(|l| serde_json::json!({ "digest": digest(l) }))
        .collect();
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "config": { "digest": digest(&config) },
        "layers": listed,
    })
    .to_string()
    .into_bytes();
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{ "digest": digest(&manifest) }],
    })
    .to_string()
    .into_bytes();
    let blob = |b: &[u8]| format!("blobs/sha256/{}", Sha256::of(b));
    let mut files = vec![
        ("index.json".to_owned(), index),
        (blob(&manifest), manifest),
        (blob(&config), config),
    ];
    files.extend(layers.iter().map(|l| (blob(l), l.clone())));
    Img {
        bytes: files_tar(&files),
        base: Base {
            digest: "sha256:base".to_owned(),
            layers: base.iter().map(|l| digest(&pack(l))).collect(),
        },
    }
}

/// The image `bytes` without the blob of the digest `digest` (`sha256:<hex>`).
pub fn without_blob(bytes: &[u8], digest: &str) -> Vec<u8> {
    use std::io::Read;
    let gone = format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));
    let mut files = Vec::new();
    for entry in tar::Archive::new(bytes).entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let path = entry.path().expect("path").to_string_lossy().into_owned();
        let mut body = Vec::new();
        entry.read_to_end(&mut body).expect("body");
        if path != gone {
            files.push((path, body));
        }
    }
    files_tar(&files)
}

/// What a path of the tree is, as tierstore's flattener reads the manifests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    File { exec: bool, bytes: Vec<u8> },
    Dir,
    Symlink(String),
}

/// The flattened entries of `tree`, with the path of each.
fn entries(tree: &Tree) -> Vec<(String, TreeEntry)> {
    let manifests: BTreeMap<String, Vec<u8>> = tree.built.manifests.iter().cloned().collect();
    let root = TreeDigest::from_hex(&tree.built.root).expect("root");
    let flat = flatten(&root, |d: &TreeDigest| {
        manifests.get(&d.to_string()).cloned().ok_or("missing")
    })
    .expect("tierstore flattens the tree");
    let mut paths: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for e in flat {
        let path = match e.parent {
            None => String::new(),
            Some(parent) => format!("{}/{}", paths[parent as usize], e.name),
        };
        paths.push(path.clone());
        out.push((path, e));
    }
    out
}

/// Every path below the root of `tree` and what it is.
pub fn listing(tree: &Tree) -> BTreeMap<String, Seen> {
    let files: BTreeMap<String, Vec<u8>> = tree
        .files
        .iter()
        .map(|f| (Sha256::of(f).to_string(), f.to_vec()))
        .collect();
    entries(tree)
        .into_iter()
        .filter(|(path, _)| !path.is_empty())
        .map(|(path, e)| {
            let seen = match e.kind {
                EntryKind::Dir { .. } => Seen::Dir,
                EntryKind::Symlink { target } => Seen::Symlink(target.to_string()),
                EntryKind::File { digest, exec, .. } => Seen::File {
                    exec,
                    bytes: files[&digest.to_string()].clone(),
                },
            };
            (path, seen)
        })
        .collect()
}

/// The entries below the root of `tree` and its depth, counted from the manifests by tierstore's
/// flattener.
pub fn counted(tree: &Tree) -> (u64, u64) {
    let all = entries(tree);
    let depth = all
        .iter()
        .map(|(path, _)| path.matches('/').count() as u64)
        .max()
        .unwrap_or(0);
    (all.len() as u64 - 1, depth)
}

/// The digest of the manifest of each top-level directory of `tree`.
pub fn top_level(tree: &Tree) -> BTreeMap<String, String> {
    entries(tree)
        .into_iter()
        .filter(|(path, _)| path.matches('/').count() == 1)
        .filter_map(|(path, e)| match e.kind {
            EntryKind::Dir { digest, .. } => Some((path, digest.to_string())),
            _ => None,
        })
        .collect()
}
