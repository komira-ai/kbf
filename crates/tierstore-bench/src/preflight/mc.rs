//! The MinIO client (`mc`) as the ops verbs drive it: one store at a time, with the admin key
//! pair in the environment of the child and never in a file or on a command line.
//!
//! `MC_HOST_<alias>` carries the endpoint with the key pair in it (`http://KEY:SECRET@host:port`,
//! each part percent-encoded), which is how `mc` takes a one-off alias without a config file.
//!
//! The scoped account's secret is not on a command line either. `mc admin user add ALIAS` with
//! no key arguments reads the access key and then the secret key, one per line, from standard
//! input when that is not a terminal (`fetchUserKeys` in mc's `cmd/admin-user-add.go`; the
//! command's own help shows `echo -e "key\nsecret" | mc admin user add ALIAS`), so
//! [`Mc::add_account`] feeds them through the child's input pipe and the secret never reaches an
//! argument vector, which every user of the machine can read.

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

/// [`QUOTA_SIZE`] in bytes.
pub const QUOTA_BYTES: u64 = 30 * 1024 * 1024 * 1024;

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
        self.run_with(what, args, None)
    }

    /// [`Self::run`], giving the child `input` on its standard input.
    fn run_with(
        &self,
        what: &str,
        args: &[&str],
        input: Option<String>,
    ) -> Result<String, BenchError> {
        run_ok(self.host, what, &self.command(args, input))
    }

    /// The command line of `mc` with `args` after the global flags.
    fn command(&self, args: &[&str], input: Option<String>) -> Cmd {
        let config_dir = self.config_dir.display().to_string();
        let mut all = vec!["--config-dir", config_dir.as_str(), "--json"];
        all.extend_from_slice(args);
        let mut cmd = Cmd::new(&self.program, &all)
            .with_env(&format!("MC_HOST_{ALIAS}"), self.alias_url.clone());
        cmd.stdin = input;
        cmd
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

    /// Whether the store has a bucket called `bucket` (`mc ls` of the alias, read-only).
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails or a line of its answer is not JSON: a store
    /// that cannot be read is not a store without the bucket.
    pub fn bucket_exists(&self, bucket: &str) -> Result<bool, BenchError> {
        let out = self.run("mc ls", &["ls", ALIAS])?;
        let wanted = format!("{bucket}/");
        Ok(json_lines("mc ls", &out)?
            .iter()
            .any(|line| line.get("key").and_then(Value::as_str) == Some(wanted.as_str())))
    }

    /// Whether the store has the bucket `bucket`, asked of the bucket alone (`mc ls` of
    /// `alias/bucket`), which needs only the right to list that bucket. A bucket that is not
    /// there and one that may not be listed both exit 1 and read the same: the caller needs the
    /// bucket and may list it, or stops.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command cannot be started.
    pub fn bucket_present(&self, bucket: &str) -> Result<bool, BenchError> {
        let cmd = self.command(&["ls", &Self::target(bucket)], None);
        let out = self
            .host
            .run(&cmd)
            .map_err(|e| BenchError::Ops(format!("mc ls: cannot start: {:?}", e.kind())))?;
        Ok(out.success)
    }

    /// The hard quota of `bucket` in bytes (`mc quota info`, which needs `admin:GetBucketQuota`
    /// and nothing that changes anything), or `None` if the answer carries no quota, a quota
    /// of 0, or a quota that is not of type `hard`.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails or a line of its answer is not JSON.
    pub fn quota_info(&self, bucket: &str) -> Result<Option<u64>, BenchError> {
        let out = self.run("mc quota info", &["quota", "info", &Self::target(bucket)])?;
        Ok(json_lines("mc quota info", &out)?
            .iter()
            .find_map(|line| {
                // `mc quota info --json` prints `status`, `bucket`, `quota` (bytes, omitted
                // when 0) and `type` (`hard`, omitted when empty): mc `cmd/quota-set.go:74-80`,
                // filled at `cmd/quota-info.go:78-83`. A quota whose type is not `hard`, or
                // whose type is not printed, is not the limit the run needs, so it fails closed.
                let hard = line.get("type").and_then(Value::as_str) == Some("hard");
                hard.then(|| line.get("quota").and_then(Value::as_u64))
                    .flatten()
            })
            .filter(|bytes| *bytes > 0))
    }

    /// Whether the store has an account whose access key id is `access_key_id` (`mc admin user
    /// list`, read-only).
    ///
    /// The list is read, not `mc admin user info`: `info` fails the same way for an account that
    /// is not there and for a store that cannot be reached, and a failure must not read as
    /// "absent".
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails or a line of its answer is not JSON.
    pub fn account_exists(&self, access_key_id: &str) -> Result<bool, BenchError> {
        let out = self.run("mc admin user list", &["admin", "user", "list", ALIAS])?;
        Ok(json_lines("mc admin user list", &out)?
            .iter()
            .any(|line| line.get("accessKey").and_then(Value::as_str) == Some(access_key_id)))
    }

    /// Makes `bucket`. It does not take `--ignore-existing`: a bucket that is there (made by
    /// someone else between the caller's check and this command) is an error, not a bucket kept.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub fn make_bucket(&self, bucket: &str) -> Result<(), BenchError> {
        let target = Self::target(bucket);
        self.run("mc mb", &["mb", &target]).map(|_| ())
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

    /// Adds the account `key`. `mc admin user add` sets the secret of an account that exists, so
    /// the caller checks [`Self::account_exists`] first. The key pair goes to the child's standard
    /// input, one value per line (see the module header), never into its arguments.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the id or the secret holds a line break (it would shift the lines
    /// the command reads) or the command fails.
    pub fn add_account(&self, key: &KeyPair) -> Result<(), BenchError> {
        let secret = key.secret_access_key.expose();
        if [key.access_key_id.as_str(), secret]
            .iter()
            .any(|v| v.contains(['\n', '\r']))
        {
            return Err(BenchError::Ops(
                "an account's key pair may not hold a line break".to_owned(),
            ));
        }
        let input = format!("{}\n{secret}\n", key.access_key_id);
        self.run_with(
            "mc admin user add",
            &["admin", "user", "add", ALIAS],
            Some(input),
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

    /// Removes the account `access_key_id`. Crate-private: the only caller is
    /// [`crate::provision::state::State::remove_account`], which removes an account only if the
    /// run made it.
    ///
    /// # Errors
    /// [`BenchError::Ops`] if the command fails.
    pub(crate) fn remove_account(&self, access_key_id: &str) -> Result<(), BenchError> {
        self.run(
            "mc admin user remove",
            &["admin", "user", "remove", ALIAS, access_key_id],
        )
        .map(|_| ())
    }
}

/// The objects of an `mc --json` answer, each parsed. Most commands print one object per line;
/// `mc quota info` prints an indented, multi-line object (`json.MarshalIndent`, mc
/// `cmd/quota-set.go:96-97`), so the answer is read as a stream of objects, not line by line.
fn json_lines(what: &str, out: &str) -> Result<Vec<Value>, BenchError> {
    serde_json::Deserializer::from_str(out)
        .into_iter::<Value>()
        .collect::<Result<_, _>>()
        .map_err(|_| BenchError::Ops(format!("{what}: the answer is not JSON")))
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
