//! An input root holding names with the OCI whiteout prefix `.wh.`: the files a build that
//! writes OCI layers outputs (a whiteout `.wh.<name>` and the opaque marker `.wh..wh..opq`, each
//! an empty file) and other `.wh.` entries (a file with bytes, a directory, a symlink, the bare
//! prefix). tierstore takes each as an ordinary name, so both writers make an action tree of
//! them, and tierstore's own readers at kbf's pin read every one back at its path, as written.

use kbf_front::action_tree::Listing;
use tierstore::EntryKind;

use super::listing::{EMPTY_HEX, file_line, list, read};
use super::*;

/// The input root: `etc/.wh.ssl` (empty: a whiteout of `etc/ssl`), `cache/.wh..wh..opq`
/// (empty: an opaque marker) beside `cache/kept`, `.wh.` (the bare prefix, with bytes),
/// `.wh.dir/inner`, and the symlink `.wh.link` to `etc`. Returns it and the digests of the two
/// files with bytes, `kept` and `.wh.`.
async fn whiteout_root(rig: &Rig) -> (Digest, Digest, Digest) {
    let empty = kbf_segments::sha256(&[]);
    let kept = rig.farm.packed("kept", "cas/f/w/p0", 0).await;
    let bare = rig.farm.packed("bare", "cas/f/w/p0", 4).await;
    let etc = rig
        .store(&directory(
            vec![file(".wh.ssl", &empty, false)],
            vec![],
            vec![],
        ))
        .await;
    let cache = rig
        .store(&directory(
            vec![
                file(".wh..wh..opq", &empty, false),
                file("kept", &kept, false),
            ],
            vec![],
            vec![],
        ))
        .await;
    let inner = rig
        .store(&directory(
            vec![file("inner", &kept, false)],
            vec![],
            vec![],
        ))
        .await;
    let root = rig
        .store(&directory(
            vec![file(".wh.", &bare, true)],
            vec![
                subdir(".wh.dir", &inner),
                subdir("cache", &cache),
                subdir("etc", &etc),
            ],
            vec![link(".wh.link", "etc")],
        ))
        .await;
    (root, kept, bare)
}

/// Catches: an input root with `.wh.` names refused as one tierstore refuses
/// (`NoTree::Refused`, the live `no put directory` of a build that outputs OCI whiteout files),
/// or written otherwise than it is: the manifests are written, and tierstore's reader flattens
/// them with each `.wh.` entry under `kbf-work` at its path, of its kind, its bytes and link
/// target as given.
#[tokio::test]
async fn an_input_root_with_whiteout_names_is_written_and_read_back() {
    let rig = Rig::new().await;
    let (root, kept, bare) = whiteout_root(&rig).await;
    let written = rig.write(&root).await.expect("an action tree");
    let entries = rig.farm.flatten(&written).expect("the reader takes it");
    let mut paths: Vec<String> = Vec::with_capacity(entries.len());
    let mut got = BTreeMap::new();
    for entry in &entries {
        let path = match entry.parent.map(|parent| paths[parent as usize].as_str()) {
            None => String::new(),
            Some("") => entry.name.to_string(),
            Some(parent) => format!("{parent}/{}", entry.name),
        };
        paths.push(path.clone());
        let Some(path) = path.strip_prefix("kbf-work/") else {
            continue;
        };
        let kind = match &entry.kind {
            EntryKind::Dir { .. } => "dir".to_owned(),
            EntryKind::File {
                digest, size, exec, ..
            } => format!("file exec={exec} size={size} sha={digest}"),
            EntryKind::Symlink { target } => format!("symlink {target}"),
        };
        assert!(got.insert(path.to_owned(), kind).is_none(), "{path} twice");
    }
    let file = |exec: bool, digest: &Digest| {
        format!(
            "file exec={exec} size={} sha={}",
            digest.size_bytes,
            digest.hash_hex()
        )
    };
    let empty = format!("file exec=false size=0 sha={EMPTY_HEX}");
    let expected: BTreeMap<String, String> = [
        (".wh.", file(true, &bare)),
        (".wh.dir", "dir".to_owned()),
        (".wh.dir/inner", file(false, &kept)),
        (".wh.link", "symlink etc".to_owned()),
        ("cache", "dir".to_owned()),
        ("cache/.wh..wh..opq", empty.clone()),
        ("cache/kept", file(false, &kept)),
        ("etc", "dir".to_owned()),
        ("etc/.wh.ssl", empty),
    ]
    .into_iter()
    .map(|(path, kind)| (path.to_owned(), kind))
    .collect();
    assert_eq!(got, expected);
}

/// Catches: an input root with `.wh.` names refused as one the list's reader refuses
/// (`NoTree::ListingRefused`) or listed otherwise than it is: the list is stored, and
/// tierstore's version 2 reader builds a tree with each `.wh.` entry under `kbf-work` at its
/// path, of its kind, mode, size, source and digest as given.
#[tokio::test]
async fn an_input_root_with_whiteout_names_is_listed_and_read_back() {
    let rig = Rig::new().await;
    let (root, kept, bare) = whiteout_root(&rig).await;
    let listing: Listing = list(&rig, &root).await.expect("a list");
    let got: BTreeMap<String, String> = read(&rig, &listing)
        .into_iter()
        .filter_map(|(path, text)| Some((path.strip_prefix("kbf-work/")?.to_owned(), text)))
        .collect();
    let empty = file_line("0644", 0, "-", EMPTY_HEX);
    let expected: BTreeMap<String, String> = [
        (
            ".wh.",
            file_line("0755", 4, "cas/f/w/p0@4", &bare.hash_hex()),
        ),
        (".wh.dir", "dir 0755".to_owned()),
        (
            ".wh.dir/inner",
            file_line("0644", 4, "cas/f/w/p0@0", &kept.hash_hex()),
        ),
        (".wh.link", "symlink etc".to_owned()),
        ("cache", "dir 0755".to_owned()),
        ("cache/.wh..wh..opq", empty.clone()),
        (
            "cache/kept",
            file_line("0644", 4, "cas/f/w/p0@0", &kept.hash_hex()),
        ),
        ("etc", "dir 0755".to_owned()),
        ("etc/.wh.ssl", empty),
    ]
    .into_iter()
    .map(|(path, text)| (path.to_owned(), text))
    .collect();
    assert_eq!(got, expected);
}
