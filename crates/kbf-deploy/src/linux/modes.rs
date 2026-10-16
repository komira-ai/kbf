//! `linux render-units --mode` and `check-units --mode`: the systemd unit of one mode of
//! the one `kbf` binary (docs/design/architecture-v2.md, sections 1 and 11), from the
//! templates in `templates/linux/modes/`, beside `kbf.slice`.
//!
//! Each unit runs `<KBF_BIN_DIR>/kbf <mode> ...`. A mode `kbf` runs today (`daemon`,
//! `all-in-one`) carries the flags its old binary's unit carries; a mode not built yet
//! (`frontend`, `storage`, `storage-and-daemon`) carries none, and its template marks
//! where its flags go with a `TODO:` comment. The flags the new modes add
//! (`--frontend`, `--advertise-addr`, the certificate flags) are such TODO comments
//! too, never guessed.
//!
//! [`check`] reads a rendered unit back. Its rules, and the defect each catches:
//!
//! - `render`: no `@KEY@` placeholder is left; with `installable`, no `<...>` one.
//! - `exec`: `ExecStart=` is `<KBF_BIN_DIR>/kbf <mode>`, this unit's mode (a unit
//!   that starts another mode, or an old binary, under this mode's name).
//! - `flags`: a mode not built yet has no word after its name (kbf takes anything there
//!   and exits 2, so a flag in its unit would be checked by nothing).
//! - `slice`: the unit is in `kbf.slice`.
//! - `delegate`: a mode that runs builds delegates `cpu`, `cpuset`, `memory` and `pids`
//!   (`Delegate=yes` delegates them all): the daemon makes its own `supervisor/` and
//!   `actions/` cgroups, and `--lease-placement=numa-node` pins each lease to one NUMA
//!   node's CPUs and memory through `cpuset`. Systemd's rules for the key: a boolean
//!   delegates every controller or none, a list adds to what earlier lines gave, an
//!   empty value starts over.
//! - `credential`: the units of a build node (`daemon`, `storage-and-daemon`) do not
//!   give it the cold store's key: no `EnvironmentFile=` naming [`COLD_STORE_KEY_FILE`],
//!   no `Environment=` setting `AWS_ACCESS_KEY_ID` or `AWS_SECRET_ACCESS_KEY`. A build
//!   node holds no credential, and its builds run as its user. (`all-in-one` holds the
//!   storage half and reads the key; `storage-and-daemon` gets it when the storage
//!   half's uid is split from the actions', design section 9.)
//!
//! [`check_on_host`] adds one rule that runs a program:
//!
//! - `parse`: a built mode's flags are ones `kbf <mode>` takes, by kbf's own parser:
//!   `<kbf> check-args <mode> <flags...>` must exit 0 (it parses as a start would and
//!   starts nothing). `<kbf>` is `--kbf-bin` when given; else the unit's own
//!   `ExecStart=` binary when it is there; else the rule is skipped with a `SKIP parse:`
//!   line (rendering on a machine with no kbf installed, such as CI), which is not a
//!   failure. A `--kbf-bin` that cannot be run is a refusal. An older kbf, without
//!   `check-args`, fails the rule with clap's "unrecognized subcommand".
//!
//! kbf-cli's tests also parse each rendered unit with kbf's parser in-process
//! (`crates/kbf-cli/tests/deploy_units.rs`): kbf-deploy does not link the server and
//! the daemon to read their flags.

use std::collections::BTreeSet;
use std::fmt;
use std::io;

use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use kbf_host::step::Cmd;

use super::check::{Failure, Report, angle_placeholder};
use super::config::LinuxConfig;
use super::render::{SLICE, values};
use crate::template::{self, TemplateError, leftovers};
use crate::unit::{UnitError, UnitFile};

/// The cold store's key file under `KBF_ETC`: `AWS_ACCESS_KEY_ID` and
/// `AWS_SECRET_ACCESS_KEY` of the bucket's service account (`minio buckets` writes it).
pub const COLD_STORE_KEY_FILE: &str = "s3.env";

/// The controllers a mode that runs builds must have delegated (rule `delegate`).
pub const DELEGATED: [&str; 4] = ["cpu", "cpuset", "memory", "pids"];

/// The variables of the cold store's key (rule `credential`).
const KEY_VARS: [&str; 2] = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"];

/// The word after `kbf` that parses a command line and starts nothing (kbf-cli's
/// `kbf_cli::CHECK_ARGS`; its tests hold the two equal).
pub const CHECK_ARGS: &str = "check-args";

/// A mode of `kbf`, as `kbf-deploy` renders its unit. The names are kbf-cli's
/// (`kbf_cli::names`); kbf-cli's tests hold the two lists equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    Frontend,
    Storage,
    Daemon,
    StorageAndDaemon,
    AllInOne,
}

impl Mode {
    /// Every mode, in `kbf --help`'s order.
    pub const ALL: [Mode; 5] = [
        Mode::Frontend,
        Mode::Storage,
        Mode::Daemon,
        Mode::StorageAndDaemon,
        Mode::AllInOne,
    ];

    /// The word after `kbf`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::Frontend => "frontend",
            Mode::Storage => "storage",
            Mode::Daemon => "daemon",
            Mode::StorageAndDaemon => "storage-and-daemon",
            Mode::AllInOne => "all-in-one",
        }
    }

    /// Whether `kbf <mode>` has a command line of its own yet. A mode that has none
    /// takes anything and exits 2, so its unit carries no flag (rule `flags`).
    #[must_use]
    pub fn is_built(self) -> bool {
        matches!(self, Mode::Daemon | Mode::AllInOne)
    }

    /// Whether the mode runs builds, and so manages its own cgroup (rule `delegate`).
    #[must_use]
    pub fn runs_builds(self) -> bool {
        matches!(self, Mode::Daemon | Mode::StorageAndDaemon | Mode::AllInOne)
    }

    /// Whether the mode is a build node without a storage half that reads the cold
    /// store's key today (rule `credential`).
    #[must_use]
    pub fn holds_no_key(self) -> bool {
        matches!(self, Mode::Daemon | Mode::StorageAndDaemon)
    }

    /// The unit's file name: `kbf-<mode>.service`.
    #[must_use]
    pub fn unit_name(self) -> String {
        format!("kbf-{}.service", self.name())
    }

    fn template(self) -> &'static str {
        match self {
            Mode::Frontend => include_str!("../../templates/linux/modes/kbf-frontend.service"),
            Mode::Storage => include_str!("../../templates/linux/modes/kbf-storage.service"),
            Mode::Daemon => include_str!("../../templates/linux/modes/kbf-daemon.service"),
            Mode::StorageAndDaemon => {
                include_str!("../../templates/linux/modes/kbf-storage-and-daemon.service")
            }
            Mode::AllInOne => include_str!("../../templates/linux/modes/kbf-all-in-one.service"),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The unit of `mode`, rendered with `config` (the values `render-units` renders the
/// converged units with); `kbf_uid` as there.
pub fn render(
    config: &LinuxConfig,
    kbf_uid: Option<u32>,
    mode: Mode,
) -> Result<String, TemplateError> {
    render_template(config, kbf_uid, mode, mode.template())
}

/// [`render`] with another template, for the tests that plant a defect in one.
pub(super) fn render_template(
    config: &LinuxConfig,
    kbf_uid: Option<u32>,
    mode: Mode,
    text: &str,
) -> Result<String, TemplateError> {
    template::render(&mode.unit_name(), text, &values(config, kbf_uid))
}

/// Check the rendered unit `text` of `mode` against the rules of this module that read
/// the text alone (all but `parse`).
pub fn check(
    config: &LinuxConfig,
    mode: Mode,
    text: &str,
    installable: bool,
) -> Result<Report, UnitError> {
    let unit = UnitFile::parse(text)?;
    Ok(report(mode, rules(config, mode, text, &unit, installable)))
}

/// [`check`], and then rule `parse`: `kbf_bin` (or the unit's own binary, when `exec`
/// shows it there) run as `<kbf> check-args <mode> <flags...>` for a built mode whose
/// `ExecStart=` passes rule `exec`. A skipped `parse` is a note in the report.
pub fn check_on_host<E: Exec>(
    exec: &E,
    config: &LinuxConfig,
    mode: Mode,
    text: &str,
    installable: bool,
    kbf_bin: Option<&HostPath>,
) -> Result<Report, ParseError> {
    let unit = UnitFile::parse(text).map_err(ParseError::Unit)?;
    let mut failures = rules(config, mode, text, &unit, installable);
    let argv = unit.exec_start();
    let mut notes = Vec::new();
    if mode.is_built() && runs_this_mode(config, mode, &argv) {
        let found = match kbf_bin {
            Some(bin) => Some(bin.clone()),
            None => on_host(exec, argv[0]),
        };
        match found {
            Some(bin) => failures.extend(parse(exec, &bin, mode, &argv[2..])?),
            None => notes.push(format!(
                "SKIP parse: no kbf binary at {}, so the flags of kbf {mode} were not \
                 parsed; give --kbf-bin PATH to parse them with another",
                argv[0]
            )),
        }
    }
    let mut r = report(mode, failures);
    r.notes = notes;
    Ok(r)
}

/// Why rule `parse` could not be checked.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Unit(UnitError),
    #[error("cannot run {bin} {CHECK_ARGS}: {source}")]
    Run { bin: HostPath, source: io::Error },
}

/// The unit's binary, unless `exec` shows nothing at its path or the path is not
/// absolute.
fn on_host<E: Exec>(exec: &E, program: &str) -> Option<HostPath> {
    let bin = HostPath::new(program).ok()?;
    // A path `exec` cannot look at is tried: the run says why it fails.
    (!matches!(exec.stat(&bin), Ok(None))).then_some(bin)
}

/// Rule `parse`: `bin check-args <mode> <flags>` exits 0, or the failure with the
/// first line it printed on stderr.
fn parse<E: Exec>(
    exec: &E,
    bin: &HostPath,
    mode: Mode,
    flags: &[&str],
) -> Result<Option<Failure>, ParseError> {
    let cmd = flags.iter().fold(
        Cmd::new(bin.clone()).arg(CHECK_ARGS).arg(mode.name()),
        |c, f| c.arg(*f),
    );
    let out = exec.query(&cmd).map_err(|source| ParseError::Run {
        bin: bin.clone(),
        source,
    })?;
    if out.status.success() {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let said = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(nothing on stderr)");
    let how = out
        .status
        .code()
        .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}"));
    Ok(Some(Failure {
        rule: "parse",
        message: format!(
            "kbf {mode} refuses the flags of {} ({bin} {CHECK_ARGS}: {how}): {said}",
            mode.unit_name()
        ),
    }))
}

/// Whether `argv` starts `<KBF_BIN_DIR>/kbf <mode>` (rule `exec`).
fn runs_this_mode(config: &LinuxConfig, mode: Mode, argv: &[&str]) -> bool {
    argv.first() == Some(&format!("{}/kbf", config.bin_dir).as_str())
        && argv.get(1) == Some(&mode.name())
}

/// The controllers the `Delegate=` lines of `[Service]` delegate, by systemd's rules
/// (a boolean delegates all or none, a list adds, an empty value starts over); `None`
/// for `Delegate=yes`, every controller.
fn delegated(unit: &UnitFile) -> Option<BTreeSet<String>> {
    let mut all = false;
    let mut listed = BTreeSet::new();
    for d in unit
        .directives()
        .iter()
        .filter(|d| d.section == "Service" && d.key == "Delegate")
    {
        let v = d.value.trim();
        if ["1", "yes", "y", "true", "t", "on"].contains(&v) {
            all = true;
            listed.clear();
        } else if v.is_empty() || ["0", "no", "n", "false", "f", "off"].contains(&v) {
            all = false;
            listed.clear();
        } else {
            listed.extend(v.split_whitespace().map(str::to_owned));
        }
    }
    (!all).then_some(listed)
}

/// What gives a build node the cold store's key, if anything does.
fn key_given(unit: &UnitFile) -> Option<String> {
    let service = unit.directives().iter().filter(|d| d.section == "Service");
    for d in service {
        match d.key.as_str() {
            // `-` makes a missing file no error; the path is the rest.
            "EnvironmentFile"
                if d.value.trim().trim_start_matches('-').rsplit('/').next()
                    == Some(COLD_STORE_KEY_FILE) =>
            {
                return Some(format!("EnvironmentFile={}", d.value.trim()));
            }
            "Environment" => {
                let mut set = d.value.split_whitespace().filter_map(|w| {
                    let name = w.trim_matches('"').split('=').next()?;
                    KEY_VARS.contains(&name).then_some(name)
                });
                if let Some(name) = set.next() {
                    return Some(format!("Environment= sets {name}"));
                }
            }
            _ => {}
        }
    }
    None
}

/// The rules of this module that read the unit alone.
fn rules(
    config: &LinuxConfig,
    mode: Mode,
    text: &str,
    unit: &UnitFile,
    installable: bool,
) -> Vec<Failure> {
    let name = mode.unit_name();
    let mut failures = Vec::new();
    let mut fail = |rule: &'static str, message: String| failures.push(Failure { rule, message });

    // render
    if let Some(p) = leftovers(text).first() {
        fail("render", format!("{name} keeps the placeholder {p}"));
    }
    if let Some(p) = angle_placeholder(text).filter(|_| installable) {
        fail("render", format!("{name} keeps a <...> placeholder: {p}"));
    }

    // exec
    let argv = unit.exec_start();
    if !runs_this_mode(config, mode, &argv) {
        fail(
            "exec",
            format!(
                "{name} starts '{}', want '{}/kbf {mode} ...'",
                argv.iter().take(2).copied().collect::<Vec<_>>().join(" "),
                config.bin_dir
            ),
        );
    }

    // flags
    if !mode.is_built() && argv.len() > 2 {
        fail(
            "flags",
            format!(
                "kbf {mode} is not built yet and takes no flag kbf can check, but {name} gives '{}'",
                argv[2..].join(" ")
            ),
        );
    }

    // slice
    if unit.get_in("Service", "Slice") != Some(SLICE) {
        fail("slice", format!("{name} is not in {SLICE}"));
    }

    // delegate
    if let Some(given) = delegated(unit).filter(|_| mode.runs_builds()) {
        let missing: Vec<&str> = DELEGATED
            .into_iter()
            .filter(|c| !given.contains(*c))
            .collect();
        if !missing.is_empty() {
            let given = if given.is_empty() {
                "no controller".to_owned()
            } else {
                given.into_iter().collect::<Vec<_>>().join(" ")
            };
            fail(
                "delegate",
                format!(
                    "{name} runs builds and delegates {given}, not {}: the daemon makes its \
                     own cgroups and needs cpu, cpuset, memory and pids delegated \
                     (Delegate=yes), cpuset to pin each lease to one NUMA node \
                     (--lease-placement=numa-node)",
                    missing.join(" ")
                ),
            );
        }
    }

    // credential
    if let Some(how) = key_given(unit).filter(|_| mode.holds_no_key()) {
        fail(
            "credential",
            format!(
                "{name} gives a build node the cold store's key ({how}): a build node holds \
                 no credential, and its builds run as its user"
            ),
        );
    }
    failures
}

/// The report of `failures` in the unit of `mode`.
fn report(mode: Mode, failures: Vec<Failure>) -> Report {
    let summary = if failures.is_empty() {
        format!("UNIT OK: {} runs kbf {mode}", mode.unit_name())
    } else {
        format!("UNIT BROKEN: {} failed check(s)", failures.len())
    };
    Report {
        failures,
        notes: Vec::new(),
        summary,
    }
}

#[cfg(test)]
#[path = "modes_tests.rs"]
mod tests;
