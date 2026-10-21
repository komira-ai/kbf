//! [`Tree`] and [`Job`]: an action's input root and the action around it, as plain data.
//!
//! The black-box suites build their actions from these and upload them through the REAPI
//! helpers of `kbf-harness` (`kbf_harness::reapi`), which turn them into messages. They hold
//! no protocol type, so a test of a platform that has no farm (the Mac legs' probes) names
//! them without linking one.

use std::time::Duration;

/// A directory of an action's input root.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    /// Files: name and contents.
    pub files: Vec<(String, Vec<u8>)>,
    /// Symbolic links: name and target.
    pub links: Vec<(String, String)>,
    /// Subdirectories: name and tree.
    pub dirs: Vec<(String, Tree)>,
}

impl Tree {
    /// `in/<name>` holding `bytes`: the shape every action here reads.
    #[must_use]
    pub fn with_input(name: &str, bytes: &[u8]) -> Self {
        Self {
            dirs: vec![(
                "in".to_owned(),
                Self {
                    files: vec![(name.to_owned(), bytes.to_vec())],
                    ..Self::default()
                },
            )],
            ..Self::default()
        }
    }
}

/// One action: its input root and the output paths it declares.
#[derive(Clone, Debug, Default)]
pub struct Job {
    /// The input root.
    pub input: Tree,
    /// The output paths the action declares.
    pub outputs: Vec<String>,
    /// The action's own timeout; none is the server's.
    pub timeout: Option<Duration>,
    /// Whether the server may serve the result from its action cache.
    pub cacheable: bool,
}
