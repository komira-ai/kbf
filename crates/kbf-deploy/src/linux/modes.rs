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
//! - `slice`: the unit is in `kbf.slice`, and a mode that runs builds has `Delegate=yes`
//!   (the daemon makes its own `supervisor/` and `actions/` cgroups).
//!
//! That a built mode's flags parse is checked against kbf's own parser, in-process, by
//! kbf-cli's tests (kbf-deploy does not link the server and the daemon to read their
//! flags): `crates/kbf-cli/tests/deploy_units.rs`.

use std::fmt;

use super::check::{Failure, Report, angle_placeholder};
use super::config::LinuxConfig;
use super::render::{SLICE, values};
use crate::template::{self, TemplateError, leftovers};
use crate::unit::UnitFile;

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

    /// Whether the mode runs builds, and so manages its own cgroup (`Delegate=yes`).
    #[must_use]
    pub fn runs_builds(self) -> bool {
        matches!(self, Mode::Daemon | Mode::StorageAndDaemon | Mode::AllInOne)
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

/// Check the rendered unit `text` of `mode` against the rules of this module.
pub fn check(
    config: &LinuxConfig,
    mode: Mode,
    text: &str,
    installable: bool,
) -> Result<Report, crate::unit::UnitError> {
    let unit = UnitFile::parse(text)?;
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
    let kbf = format!("{}/kbf", config.bin_dir);
    if argv.first() != Some(&kbf.as_str()) || argv.get(1) != Some(&mode.name()) {
        fail(
            "exec",
            format!(
                "{name} starts '{}', want '{kbf} {mode} ...'",
                argv.iter().take(2).copied().collect::<Vec<_>>().join(" ")
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
    if mode.runs_builds() && unit.get_in("Service", "Delegate") != Some("yes") {
        fail(
            "slice",
            format!("{name} runs builds and needs Delegate=yes for their cgroups"),
        );
    }

    let summary = if failures.is_empty() {
        format!("UNIT OK: {name} runs kbf {mode}")
    } else {
        format!("UNIT BROKEN: {} failed check(s)", failures.len())
    };
    Ok(Report { failures, summary })
}

#[cfg(test)]
#[path = "modes_tests.rs"]
mod tests;
