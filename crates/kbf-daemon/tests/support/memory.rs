//! A CAS in this process's memory, and actions stored in it.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use kbf_daemon::cas::{Cas, CasError, digest_of, label};
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::{Action, Command, Digest, Directory, DirectoryNode, FileNode, SymlinkNode};
use prost::Message;

/// A CAS in memory. It returns whatever bytes are stored under a digest, so a test can
/// make it lie ([`MemoryCas::corrupt`]).
#[derive(Debug, Default)]
pub struct MemoryCas {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryCas {
    pub fn insert(&self, bytes: impl Into<Vec<u8>>) -> Digest {
        let bytes = bytes.into();
        let digest = digest_of(&bytes);
        self.lock().insert(label(&digest), bytes);
        digest
    }

    pub fn message(&self, message: &impl Message) -> Digest {
        self.insert(message.encode_to_vec())
    }

    /// Replaces the bytes stored under `digest`.
    pub fn corrupt(&self, digest: &Digest, bytes: impl Into<Vec<u8>>) {
        self.lock().insert(label(digest), bytes.into());
    }

    pub fn remove(&self, digest: &Digest) {
        self.lock().remove(&label(digest));
    }

    pub fn blob(&self, digest: &Digest) -> Option<Vec<u8>> {
        self.lock().get(&label(digest)).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.blob(digest)
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(self.insert(bytes))
    }
}

/// One input file: path (slash-separated), contents, executable.
pub type Input = (&'static str, &'static [u8], bool);

/// An action to store: its argv, environment, working directory, outputs and inputs.
#[derive(Clone, Debug)]
pub struct Spec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_directory: String,
    pub outputs: Vec<String>,
    pub inputs: Vec<Input>,
    pub symlinks: Vec<(&'static str, &'static str)>,
}

impl Spec {
    /// Runs kbf-testkit's act `steps` (`;` between two), through
    /// [`super::act_program`].
    pub fn act(steps: &[&str]) -> Self {
        let program = super::act_program().display().to_string();
        Self::argv(std::iter::once(program.as_str()).chain(steps.iter().copied()))
    }

    /// Runs `/bin/sh -c script`. Only for a test of how the daemon runs a user's shell
    /// command; every other action runs [`Spec::act`].
    pub fn user_shell(script: &str) -> Self {
        Self::argv(["/bin/sh", "-c", script])
    }

    /// Runs `argv`.
    pub fn argv<'a>(argv: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            argv: argv.into_iter().map(str::to_owned).collect(),
            env: Vec::new(),
            working_directory: String::new(),
            outputs: Vec::new(),
            inputs: Vec::new(),
            symlinks: Vec::new(),
        }
    }

    pub fn outputs(mut self, outputs: &[&str]) -> Self {
        self.outputs = outputs.iter().map(|o| (*o).to_owned()).collect();
        self
    }

    pub fn inputs(mut self, inputs: &[Input]) -> Self {
        self.inputs = inputs.to_vec();
        self
    }

    /// The input root as Directory messages, each stored through `put`.
    pub fn root(&self, put: &mut impl FnMut(Vec<u8>) -> Digest) -> Digest {
        let mut node = Node::default();
        for (path, bytes, executable) in &self.inputs {
            let (dir, leaf) = node.at(path);
            dir.files.insert(leaf, (bytes.to_vec(), *executable));
        }
        for (path, target) in &self.symlinks {
            let (dir, leaf) = node.at(path);
            dir.symlinks.insert(leaf, (*target).to_owned());
        }
        node.store(put)
    }

    pub fn command(&self) -> Command {
        Command {
            arguments: self.argv.clone(),
            environment_variables: self
                .env
                .iter()
                .map(|(name, value)| EnvironmentVariable {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            output_paths: self.outputs.clone(),
            working_directory: self.working_directory.clone(),
            ..Command::default()
        }
    }

    /// Stores the whole action through `put` and returns its digest.
    pub fn store_with(&self, put: &mut impl FnMut(Vec<u8>) -> Digest) -> Digest {
        let input_root = self.root(put);
        let command = put(self.command().encode_to_vec());
        put(Action {
            command_digest: Some(command),
            input_root_digest: Some(input_root),
            ..Action::default()
        }
        .encode_to_vec())
    }

    /// Stores the whole action in `cas` and returns its digest.
    pub fn store(&self, cas: &MemoryCas) -> Digest {
        self.store_with(&mut |bytes| cas.insert(bytes))
    }
}

#[derive(Default)]
struct Node {
    files: BTreeMap<String, (Vec<u8>, bool)>,
    dirs: BTreeMap<String, Node>,
    symlinks: BTreeMap<String, String>,
}

impl Node {
    fn at(&mut self, path: &str) -> (&mut Node, String) {
        let mut parts: Vec<&str> = path.split('/').collect();
        let leaf = parts.pop().expect("a name").to_owned();
        let mut node = self;
        for part in parts {
            node = node.dirs.entry(part.to_owned()).or_default();
        }
        (node, leaf)
    }

    fn store(&self, put: &mut impl FnMut(Vec<u8>) -> Digest) -> Digest {
        let mut directory = Directory::default();
        for (name, (bytes, executable)) in &self.files {
            directory.files.push(FileNode {
                name: name.clone(),
                digest: Some(put(bytes.clone())),
                is_executable: *executable,
                ..FileNode::default()
            });
        }
        for (name, node) in &self.dirs {
            directory.directories.push(DirectoryNode {
                name: name.clone(),
                digest: Some(node.store(put)),
            });
        }
        for (name, target) in &self.symlinks {
            directory.symlinks.push(SymlinkNode {
                name: name.clone(),
                target: target.clone(),
                ..SymlinkNode::default()
            });
        }
        put(directory.encode_to_vec())
    }
}
