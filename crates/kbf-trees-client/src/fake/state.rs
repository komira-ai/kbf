//! What the fake node holds, and its rules for each call (tierstore's `node-local-trees.md`,
//! sections 3.2 to 3.6, as the trees interface answers them).

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest as _, Sha256};
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, DirEntry, Entry, EntryKind, Failure,
    GetPlacementRequest, GetPlacementResponse, Location, PlacementState, Prefetch, entry,
};
use tonic::Status;

use crate::error::Reason;
use crate::types::TreeDigest;

/// The lease of a placement made with no `ttl`: tierstore's default `trees.ttl`, 4 hours.
const DEFAULT_TTL_SECS: u32 = 4 * 3600;

/// The most children one page of a listing holds when the request says 0.
const DEFAULT_PAGE: u32 = 1000;

/// The highest mode a file or directory may have.
const MAX_MODE: u32 = 0o7777;

/// The status that answers `reason`, about `what` when it names something.
pub(super) fn refused(reason: Reason, what: Option<&str>) -> Status {
    let message = match what {
        Some(what) => format!("{}: {what}", reason.name()),
        None => reason.name().to_owned(),
    };
    Status::new(reason.code(), message)
}

/// What is wrong with one source object, as the test marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The object is gone: `source missing` naming its key.
    Missing,
    /// The object is shorter than a span of it: `short` naming the entry.
    Short,
    /// The object's bytes do not hash to an entry's `sha256`: `mismatch` naming the entry.
    Lying,
}

/// A tree the node has: its entries, sorted by path.
#[derive(Debug, Clone)]
struct Tree {
    entries: Vec<Entry>,
    files: u64,
    bytes: u64,
}

/// A placement: the tree, where it is, its state and lease.
#[derive(Debug, Clone)]
struct Placement {
    tree: TreeDigest,
    at: Vec<Location>,
    state: PlacementState,
    created_ms: u64,
    expires_ms: u64,
    /// The entries whose sources a `background` create could not read.
    failures: Vec<Failure>,
}

/// The node.
#[derive(Debug)]
pub(super) struct State {
    /// Mixed into every digest: a node of another digest builder is one with another salt.
    salt: Vec<u8>,
    trees: BTreeMap<TreeDigest, Tree>,
    placements: BTreeMap<String, Placement>,
    sources: BTreeMap<String, Source>,
    pin_limit: u64,
    next: u64,
    now_ms: u64,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            salt: b"fake-trees-v1".to_vec(),
            trees: BTreeMap::new(),
            placements: BTreeMap::new(),
            sources: BTreeMap::new(),
            pin_limit: u64::MAX,
            next: 0,
            now_ms: 0,
        }
    }

    pub(super) fn set_salt(&mut self, salt: &[u8]) {
        self.salt = salt.to_vec();
    }

    pub(super) fn set_pin_limit(&mut self, bytes: u64) {
        self.pin_limit = bytes;
    }

    pub(super) fn set_now_ms(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    pub(super) fn mark(&mut self, key: &str, source: Option<Source>) {
        match source {
            Some(source) => self.sources.insert(key.to_owned(), source),
            None => self.sources.remove(key),
        };
    }

    /// Forgets a tree, as a node does once nothing holds it: a create that names it as a child
    /// is then `unknown child tree`. False when the node did not have it.
    pub(super) fn forget_tree(&mut self, tree: TreeDigest) -> bool {
        self.trees.remove(&tree).is_some()
    }

    pub(super) fn has_tree(&self, tree: TreeDigest) -> bool {
        self.trees.contains_key(&tree)
    }

    /// The ids of the live placements, in id order.
    pub(super) fn placements(&self) -> Vec<String> {
        self.placements.keys().cloned().collect()
    }

    /// The node's digest of `entries` (sorted by path): every entry's path, kind and what the
    /// kind says (mode, `sha256` and size of a file; target of a symlink; digest of a child
    /// tree), never where a file's bytes come from.
    pub(super) fn digest(&self, entries: &[Entry]) -> TreeDigest {
        let mut hash = Sha256::new();
        hash.update(&self.salt);
        for entry in entries {
            hash.update(entry.path.as_bytes());
            hash.update([0]);
            match &entry.kind {
                Some(entry::Kind::File(file)) => {
                    hash.update(b"f");
                    hash.update(file.mode.unwrap_or(0o644).to_be_bytes());
                    hash.update(&file.sha256);
                    hash.update(file.size.to_be_bytes());
                }
                Some(entry::Kind::Dir(dir)) => {
                    hash.update(b"d");
                    hash.update(dir.mode.unwrap_or(0o755).to_be_bytes());
                }
                Some(entry::Kind::Symlink(link)) => {
                    hash.update(b"l");
                    hash.update(link.target.as_bytes());
                }
                Some(entry::Kind::Tree(child)) => {
                    hash.update(b"t");
                    hash.update(&child.tree);
                }
                None => hash.update(b"?"),
            }
            hash.update([0]);
        }
        TreeDigest::from_array(hash.finalize().into())
    }

    /// `CreateTree`, by the node's rules, in their order: the request alone, the children,
    /// the client's digest, the locations, the cache (a `wait` create only), and the sources,
    /// which only a create that fills reads: a `wait` create fails whole on the first bad one,
    /// a `background` create keeps its placement `FAILED` with the failures in it, and a
    /// `none` create reads none.
    pub(super) fn create(
        &mut self,
        request: CreateTreeRequest,
    ) -> Result<CreateTreeResponse, Status> {
        let prefetch = request.prefetch();
        let mut entries = request.entries;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        check_entries(&entries)?;
        for entry in &entries {
            if let Some(entry::Kind::Tree(child)) = &entry.kind {
                let known = TreeDigest::from_slice(&child.tree).is_some_and(|d| self.has_tree(d));
                if !known {
                    return Err(refused(Reason::UnknownChild, Some(&entry.path)));
                }
            }
        }
        let tree = self.digest(&entries);
        let claimed = request.tree.as_deref();
        if claimed.is_some_and(|claimed| claimed != tree.as_bytes()) {
            return Err(refused(Reason::DigestMismatch, None));
        }
        let prefetch = match prefetch {
            Prefetch::Unspecified => Prefetch::Wait,
            other => other,
        };
        let found = self.placed_at(&request.at, tree)?;
        let (files, bytes) = sizes(&entries);
        if prefetch == Prefetch::Wait && bytes > self.pin_limit {
            return Err(refused(Reason::TooLargeForCache, None));
        }
        let bad = match prefetch {
            Prefetch::None => Vec::new(),
            _ => self.bad_sources(&entries),
        };
        if let (Prefetch::Wait, Some((refusal, _))) = (prefetch, bad.first()) {
            return Err(refusal.clone());
        }
        let failures: Vec<Failure> = bad.into_iter().map(|(_, failure)| failure).collect();
        let ttl_ms = u64::from(request.ttl_secs.unwrap_or(DEFAULT_TTL_SECS)) * 1000;
        let expires_ms = self.now_ms + ttl_ms;
        let created = found.is_none();
        let id = match found {
            Some(id) => id,
            None => {
                self.next += 1;
                format!("p{}", self.next)
            }
        };
        let count = entries.len() as u64;
        self.trees.insert(
            tree,
            Tree {
                entries,
                files,
                bytes,
            },
        );
        let state = match prefetch {
            Prefetch::None => PlacementState::Lazy,
            Prefetch::Background if failures.is_empty() => PlacementState::Filling,
            Prefetch::Background => PlacementState::Failed,
            _ => PlacementState::Ready,
        };
        let placement = self.placements.entry(id.clone()).or_insert(Placement {
            tree,
            at: request.at.clone(),
            state,
            created_ms: self.now_ms,
            expires_ms,
            failures,
        });
        placement.expires_ms = expires_ms;
        let filled = if placement.state == PlacementState::Ready {
            files
        } else {
            0
        };
        Ok(CreateTreeResponse {
            tree: tree.as_bytes().to_vec(),
            placement: id,
            at: request.at,
            entries: count,
            files,
            bytes,
            filled,
            fill_bytes: if filled > 0 { bytes } else { 0 },
            created,
            expires_ms,
            state: placement.state.into(),
            ..CreateTreeResponse::default()
        })
    }

    /// The placement of `tree` at exactly `at`, if there is one; `location taken` if another
    /// tree is placed at any of them.
    fn placed_at(&self, at: &[Location], tree: TreeDigest) -> Result<Option<String>, Status> {
        for (id, placement) in &self.placements {
            if placement.at == at && placement.tree == tree {
                return Ok(Some(id.clone()));
            }
            if let Some(taken) = at.iter().find(|place| placement.at.contains(place)) {
                return Err(refused(Reason::LocationTaken, Some(&location_name(taken))));
            }
        }
        Ok(None)
    }

    /// Each file whose source the test marked, in path order: the refusal a `wait` create
    /// answers for it (`source missing` naming the key, `short` or `mismatch` naming the
    /// entry), and the failure a `background` placement records.
    fn bad_sources(&self, entries: &[Entry]) -> Vec<(Status, Failure)> {
        let mut bad = Vec::new();
        for entry in entries {
            let Some(entry::Kind::File(file)) = &entry.kind else {
                continue;
            };
            let key = match &file.from {
                Some(from) => from.key.clone(),
                None => format!("cas/l/{}", hex::encode(&file.sha256)),
            };
            let (reason, about) = match self.sources.get(&key) {
                Some(Source::Missing) => (Reason::SourceMissing, key),
                Some(Source::Short) => (Reason::Short, entry.path.clone()),
                Some(Source::Lying) => (Reason::Mismatch, entry.path.clone()),
                None => continue,
            };
            let failure = Failure {
                path: entry.path.clone(),
                code: reason.name().to_owned(),
            };
            bad.push((refused(reason, Some(&about)), failure));
        }
        bad
    }

    /// `GetPlacement`, with one page of a directory's children when `list` is set.
    pub(super) fn placement(
        &self,
        request: &GetPlacementRequest,
    ) -> Result<GetPlacementResponse, Status> {
        let unknown = || refused(Reason::UnknownPlacement, Some(&request.placement));
        let placement = self
            .placements
            .get(&request.placement)
            .ok_or_else(unknown)?;
        let tree = self.trees.get(&placement.tree).ok_or_else(unknown)?;
        let filled = match placement.state {
            PlacementState::Ready => tree.bytes,
            _ => 0,
        };
        let mut answer = GetPlacementResponse {
            placement: request.placement.clone(),
            tree: placement.tree.as_bytes().to_vec(),
            state: placement.state.into(),
            at: placement.at.clone(),
            files: tree.files,
            total_bytes: tree.bytes,
            filled_bytes: filled,
            created_ms: placement.created_ms,
            expires_ms: placement.expires_ms,
            failures: placement.failures.clone(),
            ..GetPlacementResponse::default()
        };
        if let Some(list) = &request.list {
            let names = children(&tree.entries, &list.path);
            let limit = match list.limit {
                0 => DEFAULT_PAGE,
                n => n,
            } as usize;
            let after = names.range::<str, _>((
                std::ops::Bound::Excluded(list.after.as_str()),
                std::ops::Bound::Unbounded,
            ));
            let page: Vec<(&String, &DirEntry)> = after.take(limit + 1).collect();
            let more = page.len() > limit;
            answer.children = page.iter().take(limit).map(|(_, e)| (*e).clone()).collect();
            if more {
                answer.next = answer
                    .children
                    .last()
                    .map(|e| e.name.clone())
                    .unwrap_or_default();
            }
        }
        Ok(answer)
    }

    /// `DeleteTree`: the placement is released; its tree stays on the node.
    pub(super) fn delete(&mut self, placement: &str) -> Result<(), Status> {
        match self.placements.remove(placement) {
            Some(_) => Ok(()),
            None => Err(refused(Reason::UnknownPlacement, Some(placement))),
        }
    }
}

/// The name of a location in a refusal: the `fs` path, or `bucket/prefix`.
fn location_name(location: &Location) -> String {
    use tierstore_trees_api::v1::location::Where;
    match &location.r#where {
        Some(Where::Fs(fs)) => fs.path.clone(),
        Some(Where::S3(s3)) => format!("{}/{}", s3.bucket, s3.prefix),
        None => String::new(),
    }
}

/// The checks of the request alone (tierstore 3.2) that the fake keeps: a path that is
/// empty or has an empty, `.` or `..` name; two entries at one path; a digest that is not 32
/// bytes; a mode above 07777; an entry of no kind.
fn check_entries(entries: &[Entry]) -> Result<(), Status> {
    let mut seen = BTreeSet::new();
    for entry in entries {
        let path = entry.path.as_str();
        let bad_name = |name: &str| name.is_empty() || name == "." || name == "..";
        if path.split('/').any(bad_name) {
            return Err(refused(Reason::UnsupportedEntryName, Some(path)));
        }
        if !seen.insert(path) {
            return Err(refused(Reason::DuplicatePath, Some(path)));
        }
        let (digest, mode) = match &entry.kind {
            Some(entry::Kind::File(file)) => (Some(&file.sha256), file.mode),
            Some(entry::Kind::Tree(child)) => (Some(&child.tree), None),
            Some(entry::Kind::Dir(dir)) => (None, dir.mode),
            Some(entry::Kind::Symlink(_)) => (None, None),
            None => {
                return Err(Status::invalid_argument(format!(
                    "entry {path} has no kind"
                )));
            }
        };
        if digest.is_some_and(|digest| digest.len() != 32) {
            return Err(refused(Reason::MalformedDigest, Some(path)));
        }
        if mode.is_some_and(|mode| mode > MAX_MODE) {
            return Err(refused(Reason::BadMode, Some(path)));
        }
    }
    Ok(())
}

/// The number of files and their bytes; a child tree's are its own, already on the node.
fn sizes(entries: &[Entry]) -> (u64, u64) {
    entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            Some(entry::Kind::File(file)) => Some(file.size),
            _ => None,
        })
        .fold((0, 0), |(files, bytes), size| (files + 1, bytes + size))
}

/// The children of the directory `dir` ("" for the root) among `entries`, by name: an entry
/// directly below it, or a directory implied by a deeper path.
fn children(entries: &[Entry], dir: &str) -> BTreeMap<String, DirEntry> {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut out = BTreeMap::new();
    for entry in entries {
        let Some(rest) = entry.path.strip_prefix(&prefix) else {
            continue;
        };
        let (name, deeper) = match rest.split_once('/') {
            Some((name, _)) => (name, true),
            None => (rest, false),
        };
        let listed = if deeper {
            dir_entry(name, EntryKind::Dir, 0o755, 0, Vec::new())
        } else {
            match &entry.kind {
                Some(entry::Kind::File(file)) => dir_entry(
                    name,
                    EntryKind::File,
                    file.mode.unwrap_or(0o644),
                    file.size,
                    file.sha256.clone(),
                ),
                Some(entry::Kind::Symlink(_)) => {
                    dir_entry(name, EntryKind::Symlink, 0o777, 0, Vec::new())
                }
                Some(entry::Kind::Dir(d)) => {
                    dir_entry(name, EntryKind::Dir, d.mode.unwrap_or(0o755), 0, Vec::new())
                }
                _ => dir_entry(name, EntryKind::Dir, 0o755, 0, Vec::new()),
            }
        };
        out.entry(name.to_owned()).or_insert(listed);
    }
    out
}

fn dir_entry(name: &str, kind: EntryKind, mode: u32, size: u64, sha256: Vec<u8>) -> DirEntry {
    DirEntry {
        name: name.to_owned(),
        kind: kind.into(),
        mode,
        size,
        sha256,
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
