//! The kit's secret files: `KEY=VALUE` lines in 0600 files of root's under `MDM_ETC`
//! (`mdm install` makes the API keys and the SCEP challenge; the operator makes
//! `dns.env`). A value read from one is a [`Secret`], which is never printed.

use std::fmt;

use super::MdmError;
use crate::linux::host;
use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};

/// A value from a secret file. It has no `Display`, and its `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: &str) -> Secret {
        Secret(value.to_owned())
    }

    /// The value, for the one place it goes (a request header, a profile, an
    /// environment file).
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
pub fn check_mode(path: &HostPath, perms: Perms, owner: Owner) -> Result<(), MdmError> {
    if perms != Perms::new(0o600) {
        return Err(MdmError::SecretMode {
            path: path.clone(),
            perms,
        });
    }
    if owner.uid != 0 {
        return Err(MdmError::SecretOwner {
            path: path.clone(),
            uid: owner.uid,
        });
    }
    Ok(())
}

/// The value of `key` in the secret file at `path`: refused unless the file is a 0600
/// file of root's holding a `key=` line with a value without control characters.
pub fn read<E: Exec>(exec: &E, path: &HostPath, key: &str) -> Result<Secret, MdmError> {
    let missing = || MdmError::NoSecret {
        path: path.clone(),
        key: key.to_owned(),
    };
    let Some(Entry::File {
        perms,
        owner,
        bytes,
    }) = host::inspect(exec, path)?
    else {
        return Err(missing());
    };
    check_mode(path, perms, owner)?;
    let prefix = format!("{key}=");
    let text = String::from_utf8_lossy(&bytes);
    let value = text
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .filter(|v| !v.is_empty())
        .ok_or_else(missing)?;
    if value.chars().any(char::is_control) {
        return Err(MdmError::SecretValue {
            path: path.clone(),
            key: key.to_owned(),
        });
    }
    Ok(Secret::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kbf_host::testkit::FakeExec;

    fn p(s: &str) -> HostPath {
        HostPath::new(s).expect("valid test path")
    }

    fn put(exec: &mut FakeExec, bytes: &[u8], perms: u32, uid: u32) {
        exec.entries.insert(
            p("/etc/kbf-mdm/x.env"),
            Entry::File {
                perms: Perms::new(perms),
                owner: Owner { uid, gid: 0 },
                bytes: bytes.to_vec(),
            },
        );
    }

    /// Catches a secret read from a file others can read or change, a value taken from
    /// another key that shares a prefix, an empty value taken as one, a value with a
    /// carriage return (it would end a request header), and a secret that prints.
    #[test]
    fn a_secret_comes_only_from_a_0600_root_file_holding_its_key() {
        let mut exec = FakeExec::new(0);
        let path = p("/etc/kbf-mdm/x.env");
        let read_it = |exec: &FakeExec| read(exec, &path, "API").map_err(|e| e.to_string());
        assert!(read_it(&exec).is_err_and(|e| e.contains("has no API=")));
        put(&mut exec, b"API_OLD=old\nAPI=\nAPI=k1\n", 0o600, 0);
        let got = read(&exec, &path, "API");
        assert!(got.is_err_and(|e| e.to_string().contains("has no API=")));
        put(&mut exec, b"API_OLD=old\nAPI=k1\n", 0o600, 0);
        let got = read(&exec, &path, "API").expect("read");
        assert_eq!(
            (got.expose(), format!("{got:?}")),
            ("k1", "Secret(..)".to_owned())
        );
        put(&mut exec, b"API=k1\n", 0o640, 0);
        assert!(read_it(&exec).is_err_and(|e| e.contains("not 0600")));
        put(&mut exec, b"API=k1\n", 0o600, 501);
        assert!(read_it(&exec).is_err_and(|e| e.contains("not root")));
        put(&mut exec, b"API=k\r1\n", 0o600, 0);
        assert!(read_it(&exec).is_err_and(|e| e.contains("control character")));
    }
}
