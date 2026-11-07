//! `kbf-deploy job-service plan`, `apply` and `back-out`: moving one frontend's executor onto
//! the job service and back (`docs/deploy/job-service-cutover.md`).
//!
//! The settings are keys of the `[executor]` table of the frontend's `kbf.toml`, one key per
//! flag of the same name. [`Settings`] is what the switch sets: `executor = "js-agent"`, the job
//! service (`executor-job-url`, `executor-job-token-file`, and `executor-job-ca-file` for an
//! `https` URL, removed for an `http` one) and the shape of the virtual workers
//! (`executor-hosts`, `executor-host-cpus`, `executor-host-memory-gib`, `executor-host-slots`).
//! Every other key of the file is kept as it is, and the executor's other needs
//! (`executor-frontend-id`, the two images, the secret delivery) must already be there:
//! [`Readiness`] names the ones that are not.
//!
//! - `plan` reads the file and prints, for each key, what it holds now and what the switch
//!   sets, the executor flags of the job-service mode, and the back-out: the flags of the
//!   mode the frontend runs now. It changes nothing and reads no token: the token file is
//!   named by its path only, so no output holds a token.
//! - `apply` and `back-out` ([`crate::cutover_apply`]) are dry runs without `--apply`.
//!
//! Exit 0 the plan is complete; 1 the file lacks something the executor needs (`FAIL: ...` on
//! stderr, after the plan); 2 refused: a flag, or a file that cannot be read or rewritten
//! (`REFUSED: ...` on stderr, nothing on stdout).

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args;
use kbf_job_client::Origin;

/// The table of `kbf.toml` that holds the executor's keys.
pub(crate) const TABLE: &str = "executor";

/// The executor the frontend runs when the file names none.
pub(crate) const DEFAULT_EXECUTOR: &str = "kbf-daemon";

/// The executor of the job service.
pub(crate) const JOB_SERVICE_EXECUTOR: &str = "js-agent";

/// The keys the switch sets, in the order they are printed and written.
pub(crate) const KEYS: [&str; 8] = [
    "executor",
    "executor-job-url",
    "executor-job-token-file",
    "executor-job-ca-file",
    "executor-hosts",
    "executor-host-cpus",
    "executor-host-memory-gib",
    "executor-host-slots",
];

/// The keys of `--executor js-agent` the switch does not set and the file must already hold.
const KEPT: [&str; 3] = [
    "executor-frontend-id",
    "executor-base-image",
    "executor-kbf-image",
];

/// The job service and the virtual workers: the flags `plan` and `apply` share.
#[derive(Args, Clone, Debug)]
pub struct Target {
    /// The frontend's `kbf.toml`, whose `[executor]` table the switch changes.
    #[arg(long, value_name = "FILE")]
    pub(crate) config: PathBuf,
    /// The job service: `https://host:port` (the lease server's `--job-listen`), or
    /// `http://` on loopback only.
    #[arg(long, value_name = "URL")]
    pub(crate) url: String,
    /// The file on the frontend's host holding kbf's client token (only its path is
    /// written; the token is never read by `plan`).
    #[arg(long, value_name = "FILE")]
    pub(crate) token_file: PathBuf,
    /// The CA certificate (PEM file) the job service's certificate chains to: needed for an
    /// `https` URL, refused for `http`.
    #[arg(long, value_name = "FILE")]
    pub(crate) ca_file: Option<PathBuf>,
    /// How many virtual workers the frontend registers (`executor-hosts`).
    #[arg(long, value_name = "WORKERS", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) hosts: u64,
    /// The cores of each virtual worker (`executor-host-cpus`).
    #[arg(long, value_name = "CORES", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) host_cpus: u64,
    /// The memory of each virtual worker, in GiB (`executor-host-memory-gib`).
    #[arg(long, value_name = "GIB", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) host_memory_gib: u64,
    /// The actions each virtual worker runs at once (`executor-host-slots`).
    #[arg(long, value_name = "SLOTS", default_value_t = 8,
          value_parser = clap::value_parser!(u64).range(1..=256))]
    pub(crate) host_slots: u64,
}

/// Why a verb of the cutover stopped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CutoverError {
    /// A bad flag, or a file that cannot be read or rewritten: nothing was changed. Exit 2.
    #[error("{0}")]
    Refused(String),
    /// A step did not succeed, or the file lacks what the executor needs. Exit 1.
    #[error("{0}")]
    Failed(String),
}

impl CutoverError {
    pub(crate) fn code(&self) -> u8 {
        match self {
            Self::Refused(_) => 2,
            Self::Failed(_) => 1,
        }
    }

    /// The error as its exit code, `REFUSED: ...` or `FAIL: ...` on `err`.
    pub(crate) fn report(&self, err: &mut dyn Write) -> ExitCode {
        let word = match self {
            Self::Refused(_) => "REFUSED",
            Self::Failed(_) => "FAIL",
        };
        let _ = writeln!(err, "{word}: {self}");
        ExitCode::from(self.code())
    }
}

/// A value of one key: a TOML string, a whole number, a switch, or no key at all.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Text(String),
    Number(i64),
    Switch(bool),
    /// Another TOML value (a table, an array, a date), kept as the file wrote it.
    Other(toml::Value),
    Absent,
}

impl Value {
    fn of(value: Option<&toml::Value>) -> Value {
        match value {
            None => Value::Absent,
            Some(toml::Value::String(s)) => Value::Text(s.clone()),
            Some(toml::Value::Integer(n)) => Value::Number(*n),
            Some(toml::Value::Boolean(b)) => Value::Switch(*b),
            Some(other) => Value::Other(other.clone()),
        }
    }

    /// The value as TOML, or `None` for an absent key.
    pub(crate) fn toml(&self) -> Option<toml::Value> {
        match self {
            Value::Text(s) => Some(toml::Value::String(s.clone())),
            Value::Number(n) => Some(toml::Value::Integer(*n)),
            Value::Switch(b) => Some(toml::Value::Boolean(*b)),
            Value::Other(v) => Some(v.clone()),
            Value::Absent => None,
        }
    }

    /// The flag that sets `key` to this value, or the words for a key that is not set.
    fn flag(&self, key: &str) -> String {
        match self {
            Value::Text(s) => format!("--{key}={s}"),
            Value::Number(n) => format!("--{key}={n}"),
            Value::Switch(true) => format!("--{key}"),
            Value::Switch(false) => format!("no --{key}"),
            Value::Other(v) => format!("--{key}={v}"),
            Value::Absent => format!("no --{key}"),
        }
    }
}

/// `absent`, or the value as TOML writes it.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.toml() {
            Some(v) => write!(f, "{v}"),
            None => f.write_str("absent"),
        }
    }
}

/// What the switch sets: one value per key of [`KEYS`], in that order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings(pub(crate) Vec<(&'static str, Value)>);

/// A path or URL as a flag value: no space, quote, backslash or control character, and no
/// `%` or `$`, so that the same text is right in `kbf.toml`, on a unit's command line and in
/// the printed flags.
fn plain(flag: &str, text: &str) -> Result<String, CutoverError> {
    let bad = |c: char| c.is_whitespace() || c.is_control() || "\"'\\%$".contains(c);
    if text.is_empty() || text.contains(bad) {
        return Err(CutoverError::Refused(format!(
            "{flag}: {text:?} is empty or holds a space, quote, backslash, control character, % or $"
        )));
    }
    Ok(text.to_owned())
}

/// An absolute path as a flag value.
fn absolute(flag: &str, path: &Path) -> Result<String, CutoverError> {
    let text = path
        .to_str()
        .ok_or_else(|| CutoverError::Refused(format!("{flag}: {} is not UTF-8", path.display())))?;
    if !path.is_absolute() {
        return Err(CutoverError::Refused(format!(
            "{flag}: {text} is not an absolute path"
        )));
    }
    plain(flag, text)
}

/// A whole number of a flag as a TOML integer.
fn number(flag: &str, n: u64) -> Result<Value, CutoverError> {
    i64::try_from(n)
        .map(Value::Number)
        .map_err(|_| CutoverError::Refused(format!("{flag}: {n} is too large")))
}

impl Settings {
    /// The settings of `t`. The URL is checked as the executor's client checks it (`https`,
    /// or `http` to loopback); the CA file is needed for `https` and refused for `http`.
    pub(crate) fn of(t: &Target) -> Result<Settings, CutoverError> {
        let url = plain("--url", &t.url)?;
        let origin: Origin = url
            .parse()
            .map_err(|e| CutoverError::Refused(format!("--url: {e}")))?;
        let ca = match (&t.ca_file, origin.is_tls()) {
            (Some(path), true) => Value::Text(absolute("--ca-file", path)?),
            (None, true) => {
                return Err(CutoverError::Refused(
                    "--url is https: give --ca-file".to_owned(),
                ));
            }
            (Some(_), false) => {
                return Err(CutoverError::Refused(
                    "--ca-file is for an https --url".to_owned(),
                ));
            }
            (None, false) => Value::Absent,
        };
        Ok(Settings(vec![
            ("executor", Value::Text(JOB_SERVICE_EXECUTOR.to_owned())),
            ("executor-job-url", Value::Text(url)),
            (
                "executor-job-token-file",
                Value::Text(absolute("--token-file", &t.token_file)?),
            ),
            ("executor-job-ca-file", ca),
            ("executor-hosts", number("--hosts", t.hosts)?),
            ("executor-host-cpus", number("--host-cpus", t.host_cpus)?),
            (
                "executor-host-memory-gib",
                number("--host-memory-gib", t.host_memory_gib)?,
            ),
            ("executor-host-slots", number("--host-slots", t.host_slots)?),
        ]))
    }

    /// The executor flags of these settings, one per line: what the frontend is started
    /// with once the switch is made. A key the switch removes is left out.
    pub(crate) fn flags(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, v)| *v != Value::Absent)
            .map(|(k, v)| v.flag(k))
            .collect()
    }
}

/// The `[executor]` table of a parsed `kbf.toml`, as values by key.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Executor(pub(crate) BTreeMap<String, toml::Value>);

impl Executor {
    /// The `[executor]` table of `text`, the content of `path`. A file that is not TOML, or
    /// whose `executor` is not a table, is refused.
    pub(crate) fn parse(path: &Path, text: &str) -> Result<Executor, CutoverError> {
        let doc: toml::Table = text.parse().map_err(|e: toml::de::Error| {
            CutoverError::Refused(format!("{} is not TOML: {}", path.display(), e.message()))
        })?;
        match doc.get(TABLE) {
            None => Ok(Executor::default()),
            Some(toml::Value::Table(t)) => Ok(Executor(
                t.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            )),
            Some(_) => Err(CutoverError::Refused(format!(
                "{}: `{TABLE}` is not a table",
                path.display()
            ))),
        }
    }

    pub(crate) fn value(&self, key: &str) -> Value {
        Value::of(self.0.get(key))
    }

    /// The executor the file names: its `executor` key, or the default.
    pub(crate) fn mode(&self) -> String {
        match self.value("executor") {
            Value::Text(s) => s,
            _ => DEFAULT_EXECUTOR.to_owned(),
        }
    }

    /// The back-out of a switch made from this table: the flag of each key the switch
    /// sets, as the file holds it now. The executor is named even when the file leaves it
    /// to the default, so that the flags say which mode the frontend returns to.
    pub(crate) fn back_out_flags(&self) -> Vec<String> {
        KEYS.iter()
            .map(|key| match (self.value(key), *key) {
                (Value::Absent, "executor") => {
                    format!("--executor={DEFAULT_EXECUTOR} (the default: the file has no key)")
                }
                (value, key) => value.flag(key),
            })
            .collect()
    }
}

/// What the executor needs that the switch does not set, as the file holds it.
#[derive(Debug, PartialEq)]
pub(crate) struct Readiness {
    /// The keys of [`KEPT`] the file has, and how the job's secrets are delivered.
    pub(crate) kept: Vec<String>,
    /// What the file lacks, each with what to add.
    pub(crate) missing: Vec<String>,
}

impl Readiness {
    pub(crate) fn of(table: &Executor) -> Readiness {
        let mut kept = Vec::new();
        let mut missing = Vec::new();
        for key in KEPT {
            match table.value(key) {
                Value::Text(s) if !s.is_empty() => kept.push(format!("{key} = {s:?}")),
                _ => missing.push(format!("{key} (a text value)")),
            }
        }
        let files = table.value("executor-secret-delivery") == Value::Text("files".to_owned());
        let argv_accepted = table.value("executor-insecure-argv-secrets") == Value::Switch(true);
        match (files, argv_accepted) {
            (true, false) => kept.push("executor-secret-delivery = \"files\"".to_owned()),
            (false, true) => kept.push(
                "executor-secret-delivery = \"argv\", accepted by \
                 executor-insecure-argv-secrets = true"
                    .to_owned(),
            ),
            (true, true) => missing.push(
                "a secret delivery the executor takes: executor-insecure-argv-secrets is \
                 refused with executor-secret-delivery = \"files\""
                    .to_owned(),
            ),
            (false, false) => missing.push(
                "the secret delivery: executor-secret-delivery = \"files\", or \
                 executor-insecure-argv-secrets = true for \"argv\""
                    .to_owned(),
            ),
        }
        Readiness { kept, missing }
    }
}

/// Print the plan of `settings` for the file `path` whose table is `table`: each key now
/// and then, the executor flags, and what is kept.
pub(crate) fn print_plan(
    path: &Path,
    table: &Executor,
    settings: &Settings,
    ready: &Readiness,
    out: &mut dyn Write,
) {
    let _ = writeln!(out, "config: [{TABLE}] in {}", path.display());
    let _ = writeln!(out, "  executor now: {}", table.mode());
    for (key, then) in &settings.0 {
        let now = table.value(key);
        let mark = if now == *then { "unchanged" } else { "->" };
        let _ = writeln!(out, "  {key}: {now} {mark} {then}");
    }
    let _ = writeln!(out, "the executor flags of the job-service mode:");
    for flag in settings.flags() {
        let _ = writeln!(out, "  {flag}");
    }
    let _ = writeln!(out, "kept from the file (the executor needs them):");
    for line in &ready.kept {
        let _ = writeln!(out, "  {line}");
    }
    for line in &ready.missing {
        let _ = writeln!(out, "  MISSING: {line}");
    }
}

/// The file at `path`, as text.
pub(crate) fn read_config(path: &Path) -> Result<String, CutoverError> {
    std::fs::read_to_string(path)
        .map_err(|e| CutoverError::Refused(format!("--config {}: {e}", path.display())))
}

/// `plan`: read the file, print the plan. Changes nothing.
pub(crate) fn plan(t: &Target, out: &mut dyn Write) -> Result<(), CutoverError> {
    let settings = Settings::of(t)?;
    let text = read_config(&t.config)?;
    let table = Executor::parse(&t.config, &text)?;
    let ready = Readiness::of(&table);
    let _ = writeln!(out, "kbf-deploy job-service plan (changes nothing)");
    print_plan(&t.config, &table, &settings, &ready, out);
    let _ = writeln!(out, "back-out: the flags of the mode it runs now:");
    for flag in table.back_out_flags() {
        let _ = writeln!(out, "  {flag}");
    }
    let _ = writeln!(
        out,
        "to switch: kbf-deploy job-service apply with these flags and the restart's (a dry \
         run without --apply)"
    );
    if ready.missing.is_empty() {
        Ok(())
    } else {
        Err(CutoverError::Failed(format!(
            "not ready: [{TABLE}] in {} lacks {}",
            t.config.display(),
            ready.missing.join("; ")
        )))
    }
}

/// Run `plan`, as an exit code.
pub(crate) fn run_plan(t: &Target, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    // A refusal prints nothing on stdout: the plan is written to a buffer first.
    let mut buf = Vec::new();
    match plan(t, &mut buf) {
        Ok(()) => {
            let _ = out.write_all(&buf);
            ExitCode::SUCCESS
        }
        Err(e @ CutoverError::Failed(_)) => {
            let _ = out.write_all(&buf);
            e.report(err)
        }
        Err(e) => e.report(err),
    }
}

#[cfg(test)]
#[path = "cutover_tests.rs"]
pub(crate) mod tests;
