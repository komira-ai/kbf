//! The units `kbf-deploy` renders, turned into the command lines of real processes: the
//! farm's way until it moved to a generated `kbf.toml` ([`crate::units`]); kept for the one
//! test that proves the move (kbf-cli's `tests/harness_toml_equivalence.rs`).
//!
//! Nothing here writes a flag by hand: a process's argv is the `ExecStart=` of the unit
//! `kbf_deploy::linux::modes::render` writes for a `config.example` whose settings name
//! this rehearsal's directories, ports and addresses. The few flags a test box cannot
//! take as rendered (no MinIO, no cgroup tree, no `/etc/subuid`) are changed by
//! [`Rendered::replace`] and [`Rendered::append`] by name, and each change is checked to
//! have something to change, so a flag that the unit stops rendering is not hidden.

use kbf_deploy::linux::modes::render;
use kbf_deploy::linux::{LinuxConfig, Mode};
use kbf_unitfile::UnitFile;

use crate::units::{Edits, Settings};

const EXAMPLE: &str = include_str!("../../kbf-deploy/templates/linux/config.example");

/// `config.example` with the settings `set` given instead of its values. A key the
/// example does not give is a mistake in the test, not a new setting.
pub fn config(set: &[(&str, String)]) -> LinuxConfig {
    let mut text = String::new();
    let mut unused: Vec<&str> = set.iter().map(|(k, _)| *k).collect();
    for line in EXAMPLE.lines() {
        let key = line.split('=').next().unwrap_or_default();
        match set
            .iter()
            .find(|(k, _)| *k == key && !line.starts_with('#'))
        {
            Some((k, v)) => {
                text.push_str(&format!("{k}={v}\n"));
                unused.retain(|u| u != k);
            }
            None => {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    assert!(unused.is_empty(), "config.example has no {unused:?}");
    LinuxConfig::parse(&text).expect("valid settings")
}

/// A unit's command line (without the binary) and environment.
pub struct Rendered {
    pub mode: Mode,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The unit of `mode` rendered with `config` for the user `uid`.
pub fn rendered(config: &LinuxConfig, uid: u32, mode: Mode) -> Rendered {
    // An unfilled `@KEY@` is an error of the renderer itself.
    let text = render(config, Some(uid), mode).expect("the unit renders");
    let unit = UnitFile::parse(&text).expect("a unit");
    let exec = unit.exec_start();
    assert!(exec[0].ends_with("/kbf"), "{exec:?}");
    assert!(names_mode(&exec[1..], mode), "{exec:?}");
    // `Environment=` lines are the unit's; `EnvironmentFile=` (the cold store's keys)
    // is read by systemd, and this rehearsal's cold store needs none.
    let env = unit
        .directives()
        .iter()
        .filter(|d| d.key == "Environment")
        .filter_map(|d| d.value.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    Rendered {
        mode,
        argv: exec[1..].iter().map(|w| (*w).to_owned()).collect(),
        env,
    }
}

/// Whether `argv` (a unit's `ExecStart=` without the binary) names `mode` as its first word,
/// `kbf storage ...`, the form every unit has today. A `--mode=<name>` anywhere else is not
/// accepted: the day `kbf` takes its mode as a flag, the units change, and this check fails
/// the farm loudly at that line, where the one edit is.
#[must_use]
pub fn names_mode(argv: &[&str], mode: Mode) -> bool {
    argv.first().is_some_and(|first| *first == mode.name())
}

/// The flag's name in `--name=value`.
fn name(flag: &str) -> &str {
    flag.split('=').next().unwrap_or(flag)
}

impl Rendered {
    /// Gives each flag in `flags` the value it names, in place of the rendered one.
    pub fn replace(mut self, flags: &[String]) -> Self {
        for flag in flags {
            let at = self.argv.iter().position(|a| name(a) == name(flag));
            let at = at.unwrap_or_else(|| panic!("{} renders no {}", self.mode, name(flag)));
            self.argv[at].clone_from(flag);
        }
        self
    }

    /// Takes out the flags named `names` (`--image`, which only the container driver
    /// takes): each must be rendered, so a flag the unit stops rendering is not hidden.
    ///
    /// # Panics
    /// When the unit renders none of a flag named.
    #[must_use]
    pub fn remove(mut self, names: &[&str]) -> Self {
        for flag in names {
            let before = self.argv.len();
            self.argv.retain(|a| name(a) != *flag);
            assert!(self.argv.len() < before, "{} renders no {flag}", self.mode);
        }
        self
    }

    /// Gives each flag in `flags` the value it names: in place of the rendered one if the
    /// unit renders the flag, else added. For limits a unit does not set but may take.
    #[must_use]
    pub fn set(mut self, flags: &[String]) -> Self {
        for flag in flags {
            match self.argv.iter().position(|a| name(a) == name(flag)) {
                Some(at) => self.argv[at].clone_from(flag),
                None => self.argv.push(flag.clone()),
            }
        }
        self
    }

    /// Adds flags the unit does not render (the test box's own).
    pub fn append(mut self, flags: &[String]) -> Self {
        for flag in flags {
            let rendered = self.argv.iter().any(|a| name(a) == name(flag));
            assert!(!rendered, "{} already renders {}", self.mode, name(flag));
            self.argv.push(flag.clone());
        }
        self
    }
}

/// The settings `config.example` takes for the process `s` describes.
pub fn settings_config(s: &Settings) -> LinuxConfig {
    let at = |name: &str| s.root.join(name).display().to_string();
    config(&[
        ("KBF_HOST_NAME", s.host.to_owned()),
        ("KBF_FRONTEND_NAME", "localhost".to_owned()),
        ("KBF_LISTEN_ADDRESS", s.host.to_owned()),
        ("KBF_NODE_ID", s.node_id.to_owned()),
        // Actions get RAM less the reserved shares: 10 GiB, over the 4 GiB an action asks by default.
        ("KBF_RAM_GIB", "16".to_owned()),
        ("MINIO_RESERVATION_GIB", "1".to_owned()),
        ("KBF_SERVER_RESERVATION_GIB", "1".to_owned()),
        ("KBF_OS_HEADROOM_GIB", "4".to_owned()),
        ("KBF_STORAGE_URL", s.wiring.storage_url.clone()),
        ("KBF_REAPI_PORT", s.wiring.reapi.to_string()),
        ("KBF_WORKER_PORT", s.wiring.worker.to_string()),
        ("KBF_API_PORT", s.wiring.api.to_string()),
        ("KBF_STORAGE_PORT", s.storage_port.to_string()),
        ("KBF_ETC", at("etc")),
        ("KBF_DATA_DIR", at("data")),
        ("KBF_META_DIR", at("meta")),
        ("KBF_HOT_DIR", at("hot")),
        ("KBF_IMAGES", s.image.to_owned()),
    ])
}

impl Edits for Rendered {
    fn base(mode: crate::units::Mode, s: &Settings, uid: u32) -> Self {
        let mode = Mode::ALL
            .into_iter()
            .find(|m| m.name() == mode.name())
            .expect("kbf-deploy renders the mode");
        rendered(&settings_config(s), uid, mode)
    }
    fn replace(self, flags: &[String]) -> Self {
        Rendered::replace(self, flags)
    }
    fn set(self, flags: &[String]) -> Self {
        Rendered::set(self, flags)
    }
    fn append(self, flags: &[String]) -> Self {
        Rendered::append(self, flags)
    }
}

#[cfg(test)]
#[path = "units_rendered_tests.rs"]
mod tests;
