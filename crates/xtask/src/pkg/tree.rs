//! The files a package installs: a tree of directories and files, relative to `/`, all
//! owned by root:wheel with the epoch as their time, so what the payload and the bill
//! of materials hold depends on nothing but the files' names, modes and bytes.

use std::collections::BTreeMap;

/// The root directory's name in a payload and a bill of materials.
pub const ROOT: &str = ".";

/// The mode of a directory unless [`Tree::dir_mode`] says another.
pub const DIR_MODE: u32 = 0o755;

/// One entry of a [`Tree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A directory and its permission bits.
    Dir { mode: u32 },
    /// A file: its permission bits and its bytes.
    File { mode: u32, bytes: Vec<u8> },
}

/// The entries of a package, by path (`usr/local/bin/tool`; the root is [`ROOT`]). A
/// file's directories are in the tree whenever the file is.
#[derive(Debug)]
pub struct Tree {
    nodes: BTreeMap<String, Node>,
}

impl Tree {
    /// A tree holding only the root.
    pub fn rooted() -> Tree {
        Tree {
            nodes: BTreeMap::from([(ROOT.to_owned(), Node::Dir { mode: DIR_MODE })]),
        }
    }

    /// Every entry, a directory before what is in it.
    pub fn nodes(&self) -> impl Iterator<Item = (&str, &Node)> {
        self.nodes.iter().map(|(p, n)| (p.as_str(), n))
    }

    /// The root and everything under it.
    pub fn count(&self) -> usize {
        self.nodes.len()
    }

    /// The bytes of all the files.
    pub fn file_bytes(&self) -> u64 {
        self.nodes
            .values()
            .map(|n| match n {
                Node::Dir { .. } => 0,
                Node::File { bytes, .. } => bytes.len() as u64,
            })
            .sum()
    }

    /// Adds the directory `path` and the directories above it.
    ///
    /// # Errors
    /// `path` is not a clean relative path, or a file is where a directory would be.
    pub fn dir(&mut self, path: &str) -> Result<(), String> {
        self.parents(path)?;
        match self
            .nodes
            .entry(path.to_owned())
            .or_insert(Node::Dir { mode: DIR_MODE })
        {
            Node::Dir { .. } => Ok(()),
            Node::File { .. } => Err(format!("{path} is a file and a directory")),
        }
    }

    /// Adds the directory `path` with permission bits `mode`, and the directories above
    /// it (which are [`DIR_MODE`] unless added by this call or another).
    ///
    /// # Errors
    /// As [`Tree::dir`]; `mode` has bits beyond the permission bits; or `path` is in the
    /// tree with another mode (a mode is stated once).
    pub fn dir_mode(&mut self, path: &str, mode: u32) -> Result<(), String> {
        if mode & !0o7777 != 0 {
            return Err(format!(
                "{path}: mode {mode:o} is more than permission bits"
            ));
        }
        self.parents(path)?;
        match self.nodes.get(path) {
            None => {
                self.nodes.insert(path.to_owned(), Node::Dir { mode });
                Ok(())
            }
            Some(Node::Dir { mode: had }) if *had == mode => Ok(()),
            Some(Node::Dir { mode: had }) => Err(format!(
                "{path} is in the package with mode {had:o} and {mode:o}"
            )),
            Some(Node::File { .. }) => Err(format!("{path} is a file and a directory")),
        }
    }

    /// Adds the file `path` with permission bits `mode`, and the directories above it.
    ///
    /// # Errors
    /// `path` is not a clean relative path, `mode` has bits beyond the permission bits
    /// and the set-id and sticky bits, or `path` is already in the tree.
    pub fn file(&mut self, path: &str, mode: u32, bytes: Vec<u8>) -> Result<(), String> {
        if mode & !0o7777 != 0 {
            return Err(format!(
                "{path}: mode {mode:o} is more than permission bits"
            ));
        }
        self.parents(path)?;
        if self.nodes.contains_key(path) {
            return Err(format!("{path} is in the package twice"));
        }
        self.nodes
            .insert(path.to_owned(), Node::File { mode, bytes });
        Ok(())
    }

    /// Adds the directories above `path`, after checking `path` is clean.
    fn parents(&mut self, path: &str) -> Result<(), String> {
        for part in path.split('/') {
            if matches!(part, "" | "." | "..") || part.chars().any(char::is_control) {
                return Err(format!("{path:?} is not a clean relative path"));
            }
        }
        for (at, _) in path.match_indices('/') {
            if let Node::File { .. } = self
                .nodes
                .entry(path[..at].to_owned())
                .or_insert(Node::Dir { mode: DIR_MODE })
            {
                return Err(format!("{} is a file and a directory", &path[..at]));
            }
        }
        Ok(())
    }
}
