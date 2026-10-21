//! A package's `Bom`, the bill of materials Installer and `pkgutil` read the files and
//! their modes, owners and checksums from.
//!
//! Apple does not document the format. This writer follows `mkbom` of bomutils
//! (GPL-2.0-or-later; read for the layout, no code taken), which `pkgutil --expand`,
//! `lsbom` and Installer accept; the CI job that installs the package checks it again
//! with Apple's tools. All numbers are big-endian.
//!
//! ```text
//! header (512 bytes): "BOMStore", version 1, number of blocks, index offset and length,
//!                     vars offset and length
//! vars:               count, then per var: block index (u32), name length (u8), name
//! blocks:             back to back
//! index:              block count, then per block (0 is null): address, length;
//!                     then a free list of count 0 and two empty pointers
//! ```
//!
//! The vars are `BomInfo`, `Paths` (a "tree" whose one leaf lists the paths: per path a
//! block of its id and its info block, and a block of its parent's id and its name),
//! and the empty trees `HLIndex`, `VIndex` and `Size64`. Paths are numbered from 1 in
//! breadth-first order, the children of a directory by name; the root `.` is 1 and its
//! parent is 0. A path's info block is: type (1 file, 2 directory), 1, 3 as u16, mode
//! (type bits included) as u16, owner, group, time, size, 1, the `cksum` CRC of a file's
//! bytes, the length of a link name (always 0 here). One leaf holds at most 256 paths;
//! a package of more is refused, since the writer does not make the tree above leaves.

use std::collections::{BTreeMap, VecDeque};

use super::cpio::{S_IFDIR, S_IFREG};
use super::sha1::cksum;
use super::tree::{Node, ROOT, Tree};

/// Paths one leaf holds.
pub const LEAF_PATHS: usize = 256;

const HEADER: usize = 512;

/// The blocks and vars of a bill of materials under construction.
struct Store {
    /// Block 0 is null: it is not in this list.
    blocks: Vec<Vec<u8>>,
    vars: Vec<(&'static str, u32)>,
}

impl Store {
    /// Adds a block; its index.
    fn block(&mut self, data: Vec<u8>) -> u32 {
        self.blocks.push(data);
        self.blocks.len() as u32
    }

    /// Adds a block and names it.
    fn var(&mut self, name: &'static str, data: Vec<u8>) {
        let index = self.block(data);
        self.vars.push((name, index));
    }

    fn write(&self) -> Vec<u8> {
        let mut vars = u32_be(self.vars.len() as u32);
        for (name, index) in &self.vars {
            vars.extend(u32_be(*index));
            vars.push(name.len() as u8);
            vars.extend_from_slice(name.as_bytes());
        }
        let first = (HEADER + vars.len()) as u32;
        let mut blocks = Vec::new();
        // Block 0, the null block, and one pointer per block.
        let mut index = u32_be(self.blocks.len() as u32 + 1);
        index.extend([0u8; 8]);
        for data in &self.blocks {
            index.extend(u32_be(first + blocks.len() as u32));
            index.extend(u32_be(data.len() as u32));
            blocks.extend_from_slice(data);
        }
        // The free list: no entries, and two empty pointers.
        index.extend([0u8; 4 + 16]);
        let mut out = b"BOMStore".to_vec();
        out.extend(u32_be(1));
        out.extend(u32_be(self.blocks.len() as u32));
        out.extend(u32_be(first + blocks.len() as u32));
        out.extend(u32_be(index.len() as u32));
        out.extend(u32_be(HEADER as u32));
        out.extend(u32_be(vars.len() as u32));
        out.resize(HEADER, 0);
        out.extend(vars);
        out.extend(blocks);
        out.extend(index);
        out
    }
}

fn u32_be(n: u32) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

/// The `tree` record of a var: its one page `child`, 4096-byte pages, `paths` paths.
fn tree_record(child: u32, page: u32, paths: u32) -> Vec<u8> {
    let mut out = b"tree".to_vec();
    out.extend(u32_be(1));
    out.extend(u32_be(child));
    out.extend(u32_be(page));
    out.extend(u32_be(paths));
    out.push(0);
    out
}

/// A leaf page: `isLeaf`, the count, no neighbours, then the pairs.
fn leaf(pairs: &[(u32, u32)]) -> Vec<u8> {
    let mut out = 1u16.to_be_bytes().to_vec();
    out.extend((pairs.len() as u16).to_be_bytes());
    out.extend([0u8; 8]);
    for (a, b) in pairs {
        out.extend(u32_be(*a));
        out.extend(u32_be(*b));
    }
    out
}

/// One path in numbering order.
struct Path<'a> {
    id: u32,
    parent: u32,
    name: &'a str,
    node: &'a Node,
}

/// The paths of `tree` as the bill numbers them.
fn numbered(tree: &Tree) -> Vec<Path<'_>> {
    let mut children: BTreeMap<&str, Vec<(&str, &str, &Node)>> = BTreeMap::new();
    let mut root = None;
    for (path, node) in tree.nodes() {
        if path == ROOT {
            root = Some(node);
            continue;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or((ROOT, path));
        children.entry(parent).or_default().push((name, path, node));
    }
    let root = root.expect("a tree holds its root");
    let mut out = vec![Path {
        id: 1,
        parent: 0,
        name: ROOT,
        node: root,
    }];
    let mut queue = VecDeque::from([(1, ROOT)]);
    while let Some((id, path)) = queue.pop_front() {
        for (name, child, node) in children.get(path).into_iter().flatten() {
            let child_id = out.len() as u32 + 1;
            out.push(Path {
                id: child_id,
                parent: id,
                name,
                node,
            });
            queue.push_back((child_id, child));
        }
    }
    out
}

/// The info block of a path.
fn info(node: &Node) -> Vec<u8> {
    let (kind, mode, size, crc) = match node {
        Node::Dir { mode } => (2u8, S_IFDIR | mode, 0, 0),
        Node::File { mode, bytes } => (
            1,
            S_IFREG | mode,
            u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            cksum(bytes),
        ),
    };
    let mut out = vec![kind, 1];
    out.extend(3u16.to_be_bytes());
    out.extend((mode as u16).to_be_bytes());
    // Owner and group root:wheel, time 0.
    out.extend([0u8; 12]);
    out.extend(u32_be(size));
    out.push(1);
    out.extend(u32_be(crc));
    out.extend(u32_be(0));
    out
}

/// The `Bom` of `tree`.
///
/// # Errors
/// `tree` has more than [`LEAF_PATHS`] paths.
pub fn bom(tree: &Tree) -> Result<Vec<u8>, String> {
    let paths = numbered(tree);
    if paths.len() > LEAF_PATHS {
        return Err(format!(
            "{} paths; a bill of materials of this writer holds {LEAF_PATHS}",
            paths.len()
        ));
    }
    let mut store = Store {
        blocks: Vec::new(),
        vars: Vec::new(),
    };
    // Version, paths, one info entry (four zero words).
    let bom_info = [1u32, paths.len() as u32 + 1, 1, 0, 0, 0, 0]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect();
    store.var("BomInfo", bom_info);
    let mut pairs = Vec::new();
    for p in &paths {
        let info_block = store.block(info(p.node));
        let mut id = u32_be(p.id);
        id.extend(u32_be(info_block));
        let id_block = store.block(id);
        let mut file = u32_be(p.parent);
        file.extend_from_slice(p.name.as_bytes());
        file.push(0);
        pairs.push((id_block, store.block(file)));
    }
    let page = store.block(leaf(&pairs));
    store.var("Paths", tree_record(page, 4096, paths.len() as u32));
    let empty = leaf(&[]);
    let child = store.block(empty.clone());
    store.var("HLIndex", tree_record(child, 4096, 0));
    let child = store.block(empty.clone());
    let v_tree = store.block(tree_record(child, 128, 0));
    let mut v_index = u32_be(1);
    v_index.extend(u32_be(v_tree));
    v_index.extend([0u8; 5]);
    store.var("VIndex", v_index);
    let child = store.block(empty);
    store.var("Size64", tree_record(child, 4096, 0));
    Ok(store.write())
}
