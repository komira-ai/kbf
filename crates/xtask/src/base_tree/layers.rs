//! The layers of the base image, applied in order onto one tree, with what a tree cannot hold
//! refused by path and kind.
//!
//! A layer is a tar. Its entries are read first, whole, and refused or kept; then its
//! whiteouts and opaque markers are applied to what the lower layers made, and then its other
//! entries, so that what a layer adds is never removed by a whiteout of its own, whatever the
//! order of its tar:
//!
//! - a regular file of size 0 named `.wh.<name>` removes `<name>` from its directory; one named
//!   `.wh..wh..opq` removes everything the lower layers put in its directory. Any other entry
//!   named `.wh.<...>` (a directory, a symlink, a file with bytes) is not a whiteout: it goes into
//!   the tree, and the tree build refuses its name ([`kbf_tree_bundle::TreeError::Name`]);
//! - a later entry replaces an earlier one at the same path; a directory over a directory keeps
//!   what the directory holds and takes the new mode;
//! - a parent that no entry made is made as a directory with no mode of its own (it is a tree's
//!   0755 directory and is not listed as normalised);
//! - a hard link becomes a separate file entry with the bytes and the mode of its target, which
//!   must be a regular file the tree holds at that moment.
//!
//! Refused (each as `<path>: <kind>`): a character or block device; a FIFO; any other entry type (a tar cannot hold a socket, and a type this reader does
//! not know is refused rather than skipped); an extended attribute (a PAX record
//! `SCHILY.xattr.*` or `LIBARCHIVE.xattr.*`); a path at or below one of [`RESERVED`]; a name or
//! link target that is not UTF-8, is empty (a link's) or has a `..` component. The set-uid,
//! set-gid and sticky bits are not refused, of a file or a directory: a tree has one owner, so
//! they grant nothing there, and dropping them only narrows access. They are dropped like the
//! other mode bits, and the path is listed as normalised with the mode it had.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::rc::Rc;

use kbf_tree_bundle::{Dir, Node, Normalised};

/// The top-level names the kbf tree makes itself (`/work` and `/.kbf` of each action tree,
/// `/.kbf-run` of `kbf-task`, `/.kbf-tools` of the base tree): an image holds none of them.
pub(super) const RESERVED: [&str; 4] = ["work", ".kbf", ".kbf-run", ".kbf-tools"];

/// The directory the release's `kbf-task` is added in, and its name there.
const TOOLS: &str = ".kbf-tools";
const TOOL: &str = "kbf-task";

/// The prefix of a whiteout's name, and the name of an opaque marker.
const WHITEOUT: &str = ".wh.";
const OPAQUE: &str = ".wh..wh..opq";

/// The prefixes of the PAX records that carry an extended attribute.
const XATTRS: [&str; 2] = ["SCHILY.xattr.", "LIBARCHIVE.xattr."];

/// The mode of a file a tree holds: executable, or not.
const EXEC: u32 = 0o755;
const PLAIN: u32 = 0o644;

/// An entry of the tree as the layers leave it, with the mode it had in the image.
enum Entry {
    File {
        mode: u32,
        bytes: Rc<[u8]>,
    },
    Dir {
        /// `None` for a directory no entry of the image made.
        mode: Option<u32>,
        children: BTreeMap<String, Entry>,
    },
    Symlink {
        target: String,
    },
}

impl Entry {
    /// A directory no entry made.
    fn implicit() -> Self {
        Entry::Dir {
            mode: None,
            children: BTreeMap::new(),
        }
    }
}

/// What a layer's entry says to do.
enum Kind {
    /// Remove the entry of this name from the directory the whiteout is in.
    Whiteout(String),
    /// Remove everything the lower layers put in the directory the marker is in.
    Opaque,
    File {
        mode: u32,
        bytes: Rc<[u8]>,
    },
    Dir {
        mode: u32,
    },
    Symlink {
        target: String,
    },
    HardLink {
        target: At,
    },
}

/// A path below the root: the directories above its last name, and the name.
struct At {
    dir: Vec<String>,
    name: String,
}

impl At {
    /// The path from the root.
    fn shown(&self) -> String {
        let dir: String = self.dir.iter().map(|d| format!("{d}/")).collect();
        format!("/{dir}{}", self.name)
    }

    /// The first name of the path, the one below the root.
    fn top(&self) -> &str {
        self.dir.first().unwrap_or(&self.name)
    }
}

/// One entry of a layer: where it is, and what it says.
struct Item {
    at: At,
    kind: Kind,
}

/// The layers applied so far.
pub(super) struct Rootfs {
    root: BTreeMap<String, Entry>,
}

/// The tree as a [`Dir`], with what the build and the bundle need beside it.
pub(super) struct Flat {
    pub dir: Dir,
    /// Every file's bytes, in the order of the tree (each file once for each place it is at).
    pub files: Vec<Rc<[u8]>>,
    /// Every path whose mode the tree does not keep, sorted by path.
    pub normalised: Vec<Normalised>,
}

/// The components of the tar path `path`: the empty and `.` ones dropped; `Err` for a `..`.
fn split(path: &str) -> Result<Vec<String>, &'static str> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("a path with a `..` component"),
            part => parts.push(part.to_owned()),
        }
    }
    Ok(parts)
}

/// The path of `parts` as a directory and a name; `None` for the root, which has no name.
fn named(mut parts: Vec<String>) -> Option<At> {
    let name = parts.pop()?;
    Some(At { dir: parts, name })
}

/// The entry at `at`.
fn lookup<'a>(mut children: &'a BTreeMap<String, Entry>, at: &At) -> Option<&'a Entry> {
    for part in &at.dir {
        let Entry::Dir { children: next, .. } = children.get(part)? else {
            return None;
        };
        children = next;
    }
    children.get(&at.name)
}

/// What the directory `dir` holds, to change; `None` when there is no directory there.
fn dir_mut<'a>(
    mut children: &'a mut BTreeMap<String, Entry>,
    dir: &[String],
) -> Option<&'a mut BTreeMap<String, Entry>> {
    for part in dir {
        let Entry::Dir { children: next, .. } = children.get_mut(part)? else {
            return None;
        };
        children = next;
    }
    Some(children)
}

/// Puts `entry` at `name` in the directory `dir` below `children`: the parents no entry made are
/// made, one that is not a directory is replaced by one, and a directory over a directory keeps
/// what it holds.
fn insert(children: &mut BTreeMap<String, Entry>, dir: &[String], name: &str, entry: Entry) {
    let Some((first, rest)) = dir.split_first() else {
        if let (Some(Entry::Dir { mode, .. }), Entry::Dir { mode: new, .. }) =
            (children.get_mut(name), &entry)
        {
            *mode = *new;
            return;
        }
        children.insert(name.to_owned(), entry);
        return;
    };
    let slot = children
        .entry(first.clone())
        .or_insert_with(Entry::implicit);
    if let Entry::Dir { children: next, .. } = slot {
        insert(next, rest, name, entry);
    } else {
        let mut next = BTreeMap::new();
        insert(&mut next, rest, name, entry);
        *slot = Entry::Dir {
            mode: None,
            children: next,
        };
    }
}

/// The target of the link `entry`, or what is wrong with it.
fn link_target<R: Read>(entry: &tar::Entry<R>) -> Result<String, &'static str> {
    let Some(raw) = entry.link_name_bytes() else {
        return Err("a link with no target");
    };
    std::str::from_utf8(&raw)
        .map(str::to_owned)
        .map_err(|_| "a link target that is not UTF-8")
}

/// What the whiteout `name`, a regular file of `size` bytes, is: `None` when it is not one.
fn whiteout(name: &str, size: u64) -> Option<Kind> {
    if size != 0 {
        return None;
    }
    if name == OPAQUE {
        return Some(Kind::Opaque);
    }
    name.strip_prefix(WHITEOUT)
        .filter(|gone| !gone.is_empty())
        .map(|gone| Kind::Whiteout(gone.to_owned()))
}

/// The entry `entry` of a layer as an [`Item`], or `None` with its problems added to `problems`
/// (or, for the root directory's own entry, with nothing to do).
fn parse<R: Read>(
    entry: &mut tar::Entry<R>,
    problems: &mut Vec<String>,
) -> io::Result<Option<Item>> {
    let raw = entry.path_bytes().into_owned();
    let Ok(name) = std::str::from_utf8(&raw) else {
        problems.push(format!(
            "{}: a name that is not UTF-8",
            String::from_utf8_lossy(&raw)
        ));
        return Ok(None);
    };
    let parts = match split(name) {
        Ok(parts) => parts,
        Err(why) => {
            problems.push(format!("{name}: {why}"));
            return Ok(None);
        }
    };
    let kind = entry.header().entry_type();
    let Some(path) = named(parts) else {
        // The root directory's own entry says nothing a tree keeps.
        if kind != tar::EntryType::Directory {
            problems.push(format!("/: the root is {kind:?}, not a directory"));
        }
        return Ok(None);
    };
    let at = path.shown();
    let mut found = Vec::new();
    if RESERVED.contains(&path.top()) {
        found.push(format!(
            "{at}: a path the kbf tree makes itself (/work, /.kbf, /.kbf-run and /.kbf-tools are not in an image)"
        ));
    }
    if let Some(records) = entry.pax_extensions()? {
        for record in records.filter_map(Result::ok) {
            if XATTRS
                .iter()
                .any(|prefix| record.key_bytes().starts_with(prefix.as_bytes()))
            {
                found.push(format!(
                    "{at}: an extended attribute ({})",
                    String::from_utf8_lossy(record.key_bytes())
                ));
            }
        }
    }
    let mode = entry.header().mode()? & 0o7777;
    let size = entry.size();
    let kind = match kind {
        tar::EntryType::Regular => match whiteout(&path.name, size) {
            Some(whiteout) => Some(whiteout),
            None => {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                Some(Kind::File {
                    mode,
                    bytes: bytes.into(),
                })
            }
        },
        tar::EntryType::Directory => Some(Kind::Dir { mode }),
        tar::EntryType::Symlink => link_target(entry)
            .map(|target| Kind::Symlink { target })
            .map_err(|why| found.push(format!("{at}: a symlink: {why}")))
            .ok(),
        tar::EntryType::Link => link_target(entry)
            .and_then(|target| split(&target))
            .and_then(|parts| named(parts).ok_or("a path with no name"))
            .map(|target| Kind::HardLink { target })
            .map_err(|why| found.push(format!("{at}: a hard link: {why}")))
            .ok(),
        tar::EntryType::Char => refuse(&mut found, &at, "a character device"),
        tar::EntryType::Block => refuse(&mut found, &at, "a block device"),
        tar::EntryType::Fifo => refuse(&mut found, &at, "a FIFO"),
        other => refuse(
            &mut found,
            &at,
            &format!(
                "an entry of type {other:?}, which a tree cannot hold (a tar cannot hold a socket, and a type this reader does not know is refused, not skipped)"
            ),
        ),
    };
    match kind {
        Some(kind) if found.is_empty() => Ok(Some(Item { at: path, kind })),
        _ => {
            problems.extend(found);
            Ok(None)
        }
    }
}

/// Adds the refusal of the entry at `at` as `kind`; there is no [`Kind`] to keep.
fn refuse(found: &mut Vec<String>, at: &str, kind: &str) -> Option<Kind> {
    found.push(format!("{at}: {kind}"));
    None
}

impl Rootfs {
    /// No layer yet.
    pub(super) fn new() -> Self {
        Self {
            root: BTreeMap::new(),
        }
    }

    /// Applies the layer `tar` (plain) on what the lower layers made; what a tree cannot hold is
    /// added to `problems`, and the tree is then not to be used.
    ///
    /// # Errors
    /// The tar cannot be read.
    pub(super) fn apply(&mut self, tar: &[u8], problems: &mut Vec<String>) -> Result<(), String> {
        let not_a_tar = |e: io::Error| format!("not a tar: {e}");
        let mut archive = tar::Archive::new(tar);
        let mut items = Vec::new();
        for entry in archive.entries().map_err(not_a_tar)? {
            let mut entry = entry.map_err(not_a_tar)?;
            if let Some(item) = parse(&mut entry, problems).map_err(not_a_tar)? {
                items.push(item);
            }
        }
        for item in &items {
            match &item.kind {
                Kind::Opaque => {
                    if let Some(held) = dir_mut(&mut self.root, &item.at.dir) {
                        held.clear();
                    }
                }
                Kind::Whiteout(gone) => {
                    if let Some(held) = dir_mut(&mut self.root, &item.at.dir) {
                        held.remove(gone);
                    }
                }
                _ => {}
            }
        }
        for item in items {
            let entry = match item.kind {
                Kind::Whiteout(_) | Kind::Opaque => continue,
                Kind::File { mode, bytes } => Entry::File { mode, bytes },
                Kind::Dir { mode } => Entry::Dir {
                    mode: Some(mode),
                    children: BTreeMap::new(),
                },
                Kind::Symlink { target } => Entry::Symlink { target },
                Kind::HardLink { target } => match lookup(&self.root, &target) {
                    Some(Entry::File { mode, bytes }) => Entry::File {
                        mode: *mode,
                        bytes: Rc::clone(bytes),
                    },
                    _ => {
                        problems.push(format!(
                            "{}: a hard link to {}, which is not a regular file the layers hold at that point",
                            item.at.shown(),
                            target.shown()
                        ));
                        continue;
                    }
                },
            };
            insert(&mut self.root, &item.at.dir, &item.at.name, entry);
        }
        Ok(())
    }

    /// Adds `/.kbf-tools/kbf-task`, executable: the release's own `kbf-task`.
    pub(super) fn add_tool(&mut self, bytes: Vec<u8>) {
        let tool = Entry::File {
            mode: EXEC,
            bytes: bytes.into(),
        };
        insert(&mut self.root, &[TOOLS.to_owned()], TOOL, tool);
    }

    /// The tree as a [`Dir`]: owners dropped, a file 0755 if any execute bit is set and else
    /// 0644, a directory 0755.
    pub(super) fn flatten(&self) -> Flat {
        let mut flat = Flat {
            dir: Dir::new(),
            files: Vec::new(),
            normalised: Vec::new(),
        };
        flat.dir = flatten(&self.root, "", &mut flat);
        flat.normalised.sort_by(|a, b| a.path.cmp(&b.path));
        flat
    }
}

/// The directory of `children`, at `path`, with its files and normalised paths added to `flat`.
fn flatten(children: &BTreeMap<String, Entry>, path: &str, flat: &mut Flat) -> Dir {
    let mut dir = Dir::new();
    for (name, entry) in children {
        let at = format!("{path}/{name}");
        let node = match entry {
            Entry::File { mode, bytes } => {
                let exec = mode & 0o111 != 0;
                note(flat, &at, *mode, if exec { EXEC } else { PLAIN });
                flat.files.push(Rc::clone(bytes));
                Node::file(bytes, exec)
            }
            Entry::Dir { mode, children } => {
                if let Some(mode) = mode {
                    note(flat, &at, *mode, EXEC);
                }
                Node::Dir(flatten(children, &at, flat))
            }
            Entry::Symlink { target } => Node::Symlink {
                target: target.clone(),
            },
        };
        dir.insert(name.clone(), node);
    }
    dir
}

/// Records the path `at` as normalised when its mode `from` is not `to`.
fn note(flat: &mut Flat, at: &str, from: u32, to: u32) {
    if from != to {
        flat.normalised.push(Normalised {
            path: at.to_owned(),
            from: format!("{from:04o}"),
            to: format!("{to:04o}"),
        });
    }
}
