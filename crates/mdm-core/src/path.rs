//! Absolute paths on the host the units name.

use std::fmt;

/// An absolute path such as `/var/lib/js-mdm`.
///
/// It starts with `/`, names at least one component and has no empty, `.` or `..`
/// component and no NUL byte, so joining a relative name to it stays inside it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbsPath(String);

/// Why a string is not an [`AbsPath`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// Does not start with `/`.
    #[error("{0:?} is not absolute")]
    NotAbsolute(String),
    /// Is `/` alone.
    #[error("{0:?} names no file or directory")]
    Empty(String),
    /// Has an empty, `.` or `..` component, or a NUL byte.
    #[error("{0:?} has an empty, `.` or `..` component, or a NUL byte")]
    Component(String),
}

impl AbsPath {
    /// `path` as an [`AbsPath`], or why it is not one.
    pub fn new(path: &str) -> Result<AbsPath, PathError> {
        let Some(rest) = path.strip_prefix('/') else {
            return Err(PathError::NotAbsolute(path.to_owned()));
        };
        if rest.is_empty() {
            return Err(PathError::Empty(path.to_owned()));
        }
        let bad = |c: &str| c.is_empty() || c == "." || c == ".." || c.contains('\0');
        if rest.split('/').any(bad) {
            return Err(PathError::Component(path.to_owned()));
        }
        Ok(AbsPath(path.to_owned()))
    }

    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AbsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
