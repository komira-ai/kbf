//! What a farm's tests need to look at on a node's disk: the check of which files hold a
//! blob whole ([`files_holding`]) and a blob larger than any chunk or buffer of the read
//! path ([`input_bytes`]). Shared by this crate's farm and kbf-cli's and
//! kbf-integration-test's farm tests.

use std::path::PathBuf;

/// An input of more bytes than any chunk or buffer of the read path (a ByteStream chunk
/// and the input cache's copy buffer are 1 MiB), none of whose 64-byte runs repeats.
pub fn input_bytes() -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..(3 << 20) + 7)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}

/// Every file under `dirs` (recursively, not following symlinks) that holds `bytes`
/// whole, each inode once: its path and size.
pub fn files_holding(dirs: &[PathBuf], bytes: &[u8]) -> Vec<(PathBuf, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let mut found = std::collections::BTreeMap::new();
    let mut todo: Vec<PathBuf> = dirs.to_vec();
    while let Some(at) = todo.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            continue;
        };
        if meta.is_dir() {
            let listed = std::fs::read_dir(&at).into_iter().flatten().flatten();
            todo.extend(listed.map(|entry| entry.path()));
            continue;
        }
        // A regular file at least as long as `bytes` (a bitwise `&`: one condition, not two).
        let big_enough = meta.is_file() & (meta.len() >= bytes.len() as u64);
        if !big_enough {
            continue;
        }
        let held = std::fs::read(&at).unwrap_or_default();
        let anchor = &bytes[..64];
        let holds = held
            .windows(64)
            .enumerate()
            .filter(|(_, run)| *run == anchor)
            .any(|(at, _)| held.get(at..at + bytes.len()) == Some(bytes));
        if holds {
            found
                .entry((meta.dev(), meta.ino()))
                .or_insert((at, meta.len()));
        }
    }
    found.into_values().collect()
}

#[cfg(test)]
#[path = "node_support_tests.rs"]
mod tests;
