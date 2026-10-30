//! The MinIO client (`mc`) as the ops verbs drive it: one store at a time, with the admin key
//! pair in the environment of the child and never in a file or on a command line.
//!
//! `MC_HOST_<alias>` carries the endpoint with the key pair in it (`http://KEY:SECRET@host:port`,
//! each part percent-encoded), which is how `mc` takes a one-off alias without a config file.
//! The commands that make an account put the account's own secret on the command line, because
//! `mc admin user add` takes it there: the verb that does it (`provision --apply`) is run by
//! engineering on the node, and the secret is the scoped account's, which reaches only the test
//! bucket.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::host::{Cmd, Host, run_ok};
use super::ops::{KeyPair, OpsFile, read_key_pair};
use crate::config::StoreSpec;
use crate::error::BenchError;
use crate::role::Role;

/// The alias every command uses.
const ALIAS: &str = "tbops";

/// The quota on a test bucket: 30 GiB, in the unit `mc` takes.
pub const QUOTA_SIZE: &str = "30GiB";

/// `text` with every byte outside `A-Z a-z 0-9 - . _ ~` as `%XX`.
#[must_use]
pub fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// A MinIO client bound to one store.
pub struct Mc<'a> {
    host: &'a dyn Host,
    program: PathBuf,
    config_dir: PathBuf,
    alias_url: String,
}

impl<'a> Mc<'a> {
    /// A client of the store at `endpoint` (`http://host[:port]`) with `admin` as its key pair.
    /// `config_dir` is where `mc` keeps its own files.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if `endpoint` does not start with `http://`.
    pub fn new(
        host: &'a dyn Host,
        program: &Path,
        config_dir: &Path,
        endpoint: &str,
        admin: &KeyPair,
    ) -> Result<Self, BenchError> {
        let rest = endpoint
            .strip_prefix("http://")
            .ok_or_else(|| BenchError::Ops("a store endpoint must be http://host[:port]".into()))?;
        let alias_url = format!(
            "http://{}:{}@{}",
            percent_encode(&admin.access_key_id),
            percent_encode(admin.secret_access_key.expose()),
            rest
        );
        Ok(Self {
            host,
            program: program.to_owned(),
            config_dir: config_dir.to_owned(),
            alias_url,
        })
    }

    /// A client of the store `spec` of `role`, with the admin key pair the ops file names for
    /// the role. `mc`'s own files go to `mc/` of `result_dir`.
    ///
    /// # Errors
    /// [`BenchError`] if the ops file names no admin key file for `role`, the file cannot be
    /// read, or the endpoint is not `http://`.
    pub fn for_role(
        host: &'a dyn Host,
        ops: &OpsFile,
        role: Role,
        spec: &StoreSpec,
        result_dir: &Path,
    ) -> Result<Self, BenchError> {
        let key_file = ops.admin.get(&role).ok_or_else(|| {
            BenchError::Ops(format!("the ops file has no admin key file for {role}"))
        })?;
        let admin = read_key_pair(host, key_file)?;
        let program = ops.mc.clone().unwrap_or_else(|| PathBuf::from("mc"));
        Self::new(
            host,
            &program,
            &result_dir.join("mc"),
            &spec.endpoint,
            &admin,
        )
    }

    /// Runs `mc` with `args` after the global flags.
    fn run(&self, what: &str, args: &[&str]) -> Result<String, BenchError> {
        let config_dir = self.config_dir.display().to_string();
        let mut all = vec!["--config-dir", config_dir.as_str(), "--json"];
        all.extend_from_slice(args);
        let cmd = Cmd::new(&self.program, &all)
            .with_env(&format!("MC_HOST_{ALIAS}"), self.alias_url.clone());
        run_ok(self.host, what, &cmd)
    }

    /// `alias/bucket`.
    fn target(bucket: &str) -> String {
        format!("{ALIAS}/{bucket}")
    }

    /// `mc admin info`: the server's answer, as JSON.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails or its output is not JSON.
    pub fn info(&self) -> Result<Value, BenchError> {
        let out = self.run("mc admin info", &["admin", "info", ALIAS])?;
        serde_json::from_str(&out)
            .map_err(|e| BenchError::Ops(format!("mc admin info: not JSON: {e}")))
    }

    /// Makes `bucket`; one that exists is left as it is.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub fn make_bucket(&self, bucket: &str) -> Result<(), BenchError> {
        let target = Self::target(bucket);
        self.run("mc mb", &["mb", "--ignore-existing", &target])
            .map(|_| ())
    }

    /// Sets the hard quota of `bucket` to [`QUOTA_SIZE`].
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub fn set_quota(&self, bucket: &str) -> Result<(), BenchError> {
        let target = Self::target(bucket);
        self.run(
            "mc quota set",
            &["quota", "set", &target, "--size", QUOTA_SIZE],
        )
        .map(|_| ())
    }

    /// Creates (or replaces) the policy `name` from the document in `file`.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub fn create_policy(&self, name: &str, file: &Path) -> Result<(), BenchError> {
        let file = file.display().to_string();
        self.run(
            "mc admin policy create",
            &["admin", "policy", "create", ALIAS, name, &file],
        )
        .map(|_| ())
    }

    /// Adds the account `key` (or sets its secret, if it exists).
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub fn add_account(&self, key: &KeyPair) -> Result<(), BenchError> {
        self.run(
            "mc admin user add",
            &[
                "admin",
                "user",
                "add",
                ALIAS,
                &key.access_key_id,
                key.secret_access_key.expose(),
            ],
        )
        .map(|_| ())
    }

    /// Attaches the policy `name` to the account `access_key_id`.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub fn attach_policy(&self, name: &str, access_key_id: &str) -> Result<(), BenchError> {
        self.run(
            "mc admin policy attach",
            &[
                "admin",
                "policy",
                "attach",
                ALIAS,
                name,
                "--user",
                access_key_id,
            ],
        )
        .map(|_| ())
    }

    /// Removes the account `access_key_id`.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub fn remove_account(&self, access_key_id: &str) -> Result<(), BenchError> {
        self.run(
            "mc admin user remove",
            &["admin", "user", "remove", ALIAS, access_key_id],
        )
        .map(|_| ())
    }
}

/// The server version in an `mc admin info` answer: the first server's `version`.
#[must_use]
pub fn server_version(info: &Value) -> Option<String> {
    info.pointer("/info/servers/0/version")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Whether any drive in an `mc admin info` answer reports that it is healing.
#[must_use]
pub fn any_drive_healing(info: &Value) -> bool {
    let servers = info.pointer("/info/servers").and_then(Value::as_array);
    servers.into_iter().flatten().any(|server| {
        let drives = server.get("drives").and_then(Value::as_array);
        drives
            .into_iter()
            .flatten()
            .any(|d| d.get("healing").and_then(Value::as_bool) == Some(true))
    })
}
