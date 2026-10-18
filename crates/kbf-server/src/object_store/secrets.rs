//! STUB (red-first): the secret files do nothing.
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use kbf_types::object_store_api::{ConnectionName, Secret};

use super::Step;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("the secret file is missing")]
    Missing,
    #[error("the secret file has mode {0:04o}: it must be 0600")]
    WideMode(u32),
    #[error("the secret file is not a regular file")]
    NotRegular,
    #[error("the secret file is not valid")]
    Invalid,
    #[error("secret file {op}: {source}")]
    Io { op: &'static str, source: io::Error },
}

#[derive(Debug)]
pub struct SecretFiles {
    dir: PathBuf,
}

impl SecretFiles {
    pub fn new(dir: &Path) -> Result<Self, SecretError> {
        Ok(Self { dir: dir.to_owned() })
    }
    #[cfg(test)]
    pub(super) fn set_failpoint(&self, _hook: impl Fn(Step) + Send + Sync + 'static) {}
    pub(super) fn reach(&self, _step: Step) {}
    pub(super) fn path(&self, name: &ConnectionName, version: u64) -> PathBuf {
        self.dir.join(format!("{name}.{version:016x}.json"))
    }
    pub(super) fn put(&self, _: &ConnectionName, _: u64, _: &Secret) -> Result<(), SecretError> {
        Ok(())
    }
    pub(super) fn read(&self, _: &ConnectionName, _: u64) -> Result<Secret, SecretError> {
        Err(SecretError::Missing)
    }
    pub(super) fn remove(&self, _: &ConnectionName, _: u64) -> Result<(), SecretError> {
        Ok(())
    }
    pub(super) fn sweep(&self, _: &BTreeSet<(String, u64)>) -> Result<usize, SecretError> {
        Ok(0)
    }
}
