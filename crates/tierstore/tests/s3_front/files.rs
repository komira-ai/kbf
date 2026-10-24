//! The files and the flags that a start of the binary needs.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use kbf_testkit::dir::TestDir;

/// The access key in the key file.
pub const BIN_KEY_ID: &str = "AKIDBIN";

pub const BIN_SECRET: &str = "bin-secret";

/// A scratch directory with a key file in it, and the flags of a start that uses it.
pub struct Files {
    /// Removes the directory when the files are dropped.
    _dir: TestDir,
    pub key: PathBuf,
}

impl Files {
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let key = dir.path().join("key");
        std::fs::write(&key, format!("{BIN_KEY_ID}\n{BIN_SECRET}\n")).unwrap();
        std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
        Self { _dir: dir, key }
    }

    /// The flags a start needs, then `extra` (flags it does not have already).
    pub fn flags(&self, extra: &[&str]) -> Vec<String> {
        let key = self.key.display().to_string();
        let mut flags: Vec<String> = [
            "--listen",
            "127.0.0.1:0",
            "--proxy-id",
            "bin",
            "--hot",
            "memory",
            "--cold",
            "memory",
            "--hot-bytes",
            "1048576",
            "--bucket",
            "tier-bucket",
            "--access-key-file",
            key.as_str(),
        ]
        .map(str::to_owned)
        .to_vec();
        flags.extend(extra.iter().map(|flag| (*flag).to_owned()));
        flags
    }
}

/// `flags` with the value of `flag` changed.
pub fn with(flags: &[String], flag: &str, value: &str) -> Vec<String> {
    let mut flags = flags.to_vec();
    let at = flags.iter().position(|f| f == flag).expect("a flag");
    flags[at + 1] = value.to_owned();
    flags
}
