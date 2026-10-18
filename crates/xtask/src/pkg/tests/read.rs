//! A second implementation of the three formats, for tests: it reads what the writers
//! wrote, following the formats' descriptions rather than the writers' code, and
//! refuses what is inconsistent (a checksum, a length, an offset out of range).

use std::collections::BTreeMap;
use std::io::Read;

use super::super::sha1::{sha1, sha1_hex};

fn be32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let w = bytes
        .get(at..at + 4)
        .ok_or_else(|| format!("no u32 at {at}"))?;
    Ok(u32::from_be_bytes([w[0], w[1], w[2], w[3]]))
}

fn be16(bytes: &[u8], at: usize) -> Result<u16, String> {
    let w = bytes
        .get(at..at + 2)
        .ok_or_else(|| format!("no u16 at {at}"))?;
    Ok(u16::from_be_bytes([w[0], w[1]]))
}

/// The text between `open` and `close` in `s`.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Result<&'a str, String> {
    let start = s.find(open).ok_or_else(|| format!("no {open}"))? + open.len();
    let end = s[start..]
        .find(close)
        .ok_or_else(|| format!("no {close}"))?;
    Ok(&s[start..start + end])
}

/// The members of a xar archive by name, each checked against its table's lengths and
/// checksums, and the table against the checksum at the heap's start.
pub fn xar(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    if bytes.get(..4) != Some(b"xar!") {
        return Err("not a xar".to_owned());
    }
    let header = (be16(bytes, 4)?, be16(bytes, 6)?, be32(bytes, 24)?);
    if header != (28, 1, 1) {
        return Err(format!("header {header:?}, not (28, 1, 1)"));
    }
    let long = |at: usize| {
        Ok::<usize, String>(
            (u64::from(be32(bytes, at)?) << 32 | u64::from(be32(bytes, at + 4)?)) as usize,
        )
    };
    let (toc_z_len, toc_len) = (long(8)?, long(16)?);
    let toc_z = bytes
        .get(28..28 + toc_z_len)
        .ok_or("toc runs past the end")?;
    let heap = &bytes[28 + toc_z_len..];
    if heap.get(..20) != Some(&sha1(toc_z)[..]) {
        return Err("the toc does not match its checksum".to_owned());
    }
    let mut toc = String::new();
    flate2::read::ZlibDecoder::new(toc_z)
        .read_to_string(&mut toc)
        .map_err(|e| e.to_string())?;
    if toc.len() != toc_len {
        return Err(format!("toc is {} bytes, not {toc_len}", toc.len()));
    }
    let mut members = BTreeMap::new();
    for file in toc.split("<file id=").skip(1) {
        let name = between(file, "<name>", "</name>")?;
        let num = |tag: &str| {
            between(file, &format!("<{tag}>"), &format!("</{tag}>"))?
                .parse::<usize>()
                .map_err(|e| format!("{name}: {tag}: {e}"))
        };
        let (offset, length, size) = (num("offset")?, num("length")?, num("size")?);
        let data = heap
            .get(offset..offset + length)
            .ok_or_else(|| format!("{name} runs past the end"))?;
        if length != size {
            return Err(format!("{name}: stored and real size differ"));
        }
        for tag in ["archived-checksum", "extracted-checksum"] {
            let want = between(
                file,
                &format!("<{tag} style=\"sha1\">"),
                &format!("</{tag}>"),
            )?;
            if want != sha1_hex(data) {
                return Err(format!("{name}: {tag} does not match"));
            }
        }
        if members.insert(name.to_owned(), data.to_vec()).is_some() {
            return Err(format!("{name} twice"));
        }
    }
    Ok(members)
}

/// One cpio member.
#[derive(Debug, PartialEq, Eq)]
pub struct Cpio {
    pub name: String,
    pub inode: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub links: u32,
    pub mtime: u32,
    pub bytes: Vec<u8>,
}

/// The members of a gzip-compressed odc archive, before its trailer.
pub fn cpio(gz: &[u8]) -> Result<Vec<Cpio>, String> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let head = raw.get(at..at + 76).ok_or("header runs past the end")?;
        let text = std::str::from_utf8(head).map_err(|e| e.to_string())?;
        if &text[..6] != "070707" {
            return Err(format!("magic {:?}", &text[..6]));
        }
        let field = |from: usize, len: usize| {
            u32::from_str_radix(&text[from..from + len], 8).map_err(|e| e.to_string())
        };
        let name_len = field(59, 6)? as usize;
        let size = field(65, 11)? as usize;
        let name_end = at + 76 + name_len;
        let name = raw.get(at + 76..name_end).ok_or("name runs past the end")?;
        if name.last() != Some(&0) {
            return Err("name without NUL".to_owned());
        }
        let name = String::from_utf8(name[..name.len() - 1].to_vec()).map_err(|e| e.to_string())?;
        let bytes = raw
            .get(name_end..name_end + size)
            .ok_or("data runs past the end")?
            .to_vec();
        at = name_end + size;
        if name == "TRAILER!!!" {
            return if at == raw.len() {
                Ok(out)
            } else {
                Err("bytes after the trailer".to_owned())
            };
        }
        out.push(Cpio {
            name,
            inode: field(12, 6)?,
            mode: field(18, 6)?,
            uid: field(24, 6)?,
            gid: field(30, 6)?,
            links: field(36, 6)?,
            mtime: field(48, 11)?,
            bytes,
        });
    }
}

/// One path of a bill of materials.
#[derive(Debug, PartialEq, Eq)]
pub struct BomPath {
    pub id: u32,
    pub parent: u32,
    pub name: String,
    pub kind: u8,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    pub size: u32,
    pub cksum: u32,
}

/// The paths of a bill, in the order its leaf lists them, and the number of paths its
/// `BomInfo` states, after checking every var and block lies where the header says.
pub fn bom(bytes: &[u8]) -> Result<(Vec<BomPath>, u32), String> {
    if bytes.get(..8) != Some(b"BOMStore") || be32(bytes, 8)? != 1 {
        return Err("not a bill of materials".to_owned());
    }
    let blocks_total = be32(bytes, 12)? as usize;
    let (index_at, index_len) = (be32(bytes, 16)? as usize, be32(bytes, 20)? as usize);
    let (vars_at, vars_len) = (be32(bytes, 24)? as usize, be32(bytes, 28)? as usize);
    if index_at + index_len != bytes.len() {
        return Err("the index does not end the file".to_owned());
    }
    let count = be32(bytes, index_at)? as usize;
    if count != blocks_total + 1 {
        return Err(format!("{count} pointers for {blocks_total} blocks"));
    }
    let block = |i: u32| -> Result<&[u8], String> {
        let at = index_at + 4 + 8 * i as usize;
        let (addr, len) = (be32(bytes, at)? as usize, be32(bytes, at + 4)? as usize);
        bytes
            .get(addr..addr + len)
            .ok_or_else(|| format!("block {i} runs past the end"))
    };
    if !block(0)?.is_empty() {
        return Err("block 0 is not null".to_owned());
    }
    // The free list: no entries, two empty pointers.
    if bytes[index_at + 4 + 8 * count..] != [0u8; 20] {
        return Err("free list".to_owned());
    }
    let mut vars = BTreeMap::new();
    let mut at = vars_at + 4;
    for _ in 0..be32(bytes, vars_at)? {
        let len = *bytes.get(at + 4).ok_or("var runs past the end")? as usize;
        let name = std::str::from_utf8(&bytes[at + 5..at + 5 + len]).map_err(|e| e.to_string())?;
        vars.insert(name.to_owned(), be32(bytes, at)?);
        at += 5 + len;
    }
    if at != vars_at + vars_len {
        return Err("vars do not fill their length".to_owned());
    }
    let names: Vec<_> = vars.keys().map(String::as_str).collect();
    if names != ["BomInfo", "HLIndex", "Paths", "Size64", "VIndex"] {
        return Err(format!("vars {names:?}"));
    }
    let info = block(vars["BomInfo"])?;
    if (info.len(), be32(info, 0)?, be32(info, 8)?) != (28, 1, 1) {
        return Err("BomInfo".to_owned());
    }
    let tree = block(vars["Paths"])?;
    let (page, paths) = (be32(tree, 8)?, be32(tree, 16)?);
    if (&tree[..4], be32(tree, 4)?, be32(tree, 12)?, tree.len()) != (&b"tree"[..], 1, 4096, 21) {
        return Err("Paths tree".to_owned());
    }
    let leaf = block(page)?;
    let n = be16(leaf, 2)? as usize;
    if (be16(leaf, 0)?, be32(leaf, 4)?, be32(leaf, 8)?, leaf.len()) != (1, 0, 0, 12 + 8 * n) {
        return Err("Paths leaf".to_owned());
    }
    if n as u32 != paths || be32(info, 4)? != paths + 1 {
        return Err("path counts disagree".to_owned());
    }
    let mut out = Vec::new();
    for i in 0..n {
        let (a, b) = (be32(leaf, 12 + 8 * i)?, be32(leaf, 16 + 8 * i)?);
        let id_block = block(a)?;
        let info2 = block(be32(id_block, 4)?)?;
        let file = block(b)?;
        if (id_block.len(), info2.len(), file.last()) != (8, 31, Some(&0)) {
            return Err(format!("path {i}: block sizes"));
        }
        if (info2[1], be16(info2, 2)?, info2[22], be32(info2, 27)?) != (1, 3, 1, 0) {
            return Err(format!("path {i}: constants of the info block"));
        }
        out.push(BomPath {
            id: be32(id_block, 0)?,
            parent: be32(file, 0)?,
            name: String::from_utf8(file[4..file.len() - 1].to_vec()).map_err(|e| e.to_string())?,
            kind: info2[0],
            mode: be16(info2, 4)?,
            uid: be32(info2, 6)?,
            gid: be32(info2, 10)?,
            mtime: be32(info2, 14)?,
            size: be32(info2, 18)?,
            cksum: be32(info2, 23)?,
        });
    }
    for var in ["HLIndex", "Size64"] {
        let empty = block(vars[var])?;
        let page = block(be32(empty, 8)?)?;
        if (be32(empty, 16)?, page.len()) != (0, 12) {
            return Err(format!("{var} is not an empty tree"));
        }
    }
    Ok((out, be32(info, 4)?))
}
