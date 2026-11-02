//! The kit's secret files: `KEY=VALUE` lines in 0600 files of root's under `MDM_ETC`
//! (`install` makes the API keys and the SCEP challenge; the operator makes `dns.env`). A value
//! read from one is a [`Secret`], which is never printed.

use std::fmt;

use host_step::Host;

use crate::MdmError;

/// A value from a secret file. It has no `Display`, and its `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The value, for the one place it goes.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Refuse a secret file that is not mode 0600 or not root's.
pub fn check_mode(path: &str, mode: u32, uid: u32) -> Result<(), MdmError> {
    match (mode, uid) {
        (0o600, 0) => Ok(()),
        (mode, uid) => Err(MdmError::SecretMode {
            path: path.to_owned(),
            mode,
            uid,
        }),
    }
}

/// A `KEY=<value>` line with a value is in `bytes`.
pub fn has_value(bytes: &[u8], key: &str) -> bool {
    let prefix = format!("{key}=");
    String::from_utf8_lossy(bytes)
        .lines()
        .any(|l| l.strip_prefix(&prefix).is_some_and(|v| !v.is_empty()))
}

/// Whether `bytes` holds a line other than `KEY=` lines of `key` and blank ones.
pub fn holds_other_lines(bytes: &[u8], key: &str) -> bool {
    let prefix = format!("{key}=");
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .any(|l| !l.starts_with(&prefix))
}

/// The value of `key` in the secret file at `path`: refused unless the file is a 0600 file of
/// root's holding a `key=` line with a value without control characters.
pub fn read(host: &dyn Host, path: &str, key: &str) -> Result<Secret, MdmError> {
    let missing = || MdmError::NoSecret {
        path: path.to_owned(),
        key: key.to_owned(),
    };
    let meta = host.stat(path)?.ok_or_else(missing)?;
    check_mode(path, meta.mode, meta.uid)?;
    let bytes = host.read(path)?.ok_or_else(missing)?;
    let prefix = format!("{key}=");
    let text = String::from_utf8_lossy(&bytes);
    let value = text
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .filter(|v| !v.is_empty())
        .ok_or_else(missing)?;
    match value.chars().any(char::is_control) {
        true => Err(MdmError::SecretValue {
            path: path.to_owned(),
            key: key.to_owned(),
        }),
        false => Ok(Secret(value.to_owned())),
    }
}
