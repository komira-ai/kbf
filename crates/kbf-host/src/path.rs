//! Host paths and the root they are joined to.

use std::fmt;
use std::path::{Path, PathBuf};

/// An absolute path on the host a step changes, such as `/etc/kbf/kbf-daemon.env`.
///
/// It starts with `/`, names at least one component and has no empty, `.` or `..`
/// component, so joining it to a [`Root`] always lands inside that root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostPath(String);

/// Why a string is not a [`HostPath`] or a directory is not a [`Root`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("{0:?} is not absolute")]
    NotAbsolute(String),
    #[error("{0:?} names no file or directory")]
    Empty(String),
    #[error("{0:?} has an empty, `.` or `..` component")]
    Component(String),
    #[error("{0:?} holds a NUL byte")]
    Nul(String),
}

impl HostPath {
    pub fn new(path: &str) -> Result<HostPath, PathError> {
        let Some(rest) = path.strip_prefix('/') else {
            return Err(PathError::NotAbsolute(path.to_owned()));
        };
        if rest.is_empty() {
            return Err(PathError::Empty(path.to_owned()));
        }
        if path.contains('\0') {
            return Err(PathError::Nul(path.to_owned()));
        }
        if rest
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(PathError::Component(path.to_owned()));
        }
        Ok(HostPath(path.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The directory every [`HostPath`] is joined to: `/` on a host, a scratch directory in
/// a test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root(PathBuf);

impl Root {
    /// The host's own `/`.
    pub fn host() -> Root {
        Root(PathBuf::from("/"))
    }

    /// `dir` as the root; it must be absolute.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Root, PathError> {
        let dir = dir.into();
        if dir.is_absolute() {
            Ok(Root(dir))
        } else {
            Err(PathError::NotAbsolute(dir.display().to_string()))
        }
    }

    pub fn join(&self, path: &HostPath) -> PathBuf {
        self.0.join(&path.0[1..])
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a path check that lets a step reach outside its root (`..`), write to
    /// `/` itself, or take a relative path that would depend on the working directory.
    #[test]
    fn a_host_path_is_absolute_and_stays_inside_its_root() {
        type Variant = fn(String) -> PathError;
        let refused: [(&str, Variant); 9] = [
            ("etc/kbf", PathError::NotAbsolute),
            ("", PathError::NotAbsolute),
            ("/", PathError::Empty),
            ("/etc/../root", PathError::Component),
            ("/..", PathError::Component),
            ("/etc/./kbf", PathError::Component),
            ("//etc", PathError::Component),
            ("/etc/kbf/", PathError::Component),
            ("/etc/k\0bf", PathError::Nul),
        ];
        for (path, want) in refused {
            assert_eq!(HostPath::new(path), Err(want(path.to_owned())), "{path:?}");
        }
        let ok = HostPath::new("/etc/kbf/kbf-daemon.env").expect("a plain absolute path");
        assert_eq!(ok.as_str(), "/etc/kbf/kbf-daemon.env");
        assert_eq!(ok.to_string(), "/etc/kbf/kbf-daemon.env");
    }

    /// Catches a join that drops the root (`Path::join` with an absolute path replaces
    /// the base, so the leading `/` must be stripped) and a relative root.
    #[test]
    fn a_path_is_joined_under_the_root() {
        let p = HostPath::new("/etc/kbf/x").expect("valid");
        let root = Root::new("/scratch/r").expect("absolute");
        assert_eq!(root.join(&p), PathBuf::from("/scratch/r/etc/kbf/x"));
        assert_eq!(root.as_path(), Path::new("/scratch/r"));
        assert_eq!(Root::host().join(&p), PathBuf::from("/etc/kbf/x"));
        assert_eq!(
            Root::new("scratch"),
            Err(PathError::NotAbsolute("scratch".to_owned()))
        );
    }
}
