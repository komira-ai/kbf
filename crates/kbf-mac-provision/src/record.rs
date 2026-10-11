//! The record of the power and software update settings as `apply` first found them,
//! and `revert`, which puts them back.
//!
//! Before the first change to any of these settings, `apply` writes every one of them
//! (not only the one it changes) to [`RECORD`], root's, mode 600. Later runs keep that
//! file, so it holds the values from before kbf changed anything, however many applies
//! ran since. `revert` checks the whole record, restores each setting that differs, and
//! moves the record aside, so the next `apply` records afresh.
//!
//! A line is `<kind>\t<domain>\t<key>\t<value>`:
//! `pmset\t-\tsleep\t1` or
//! `pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticDownload\tABSENT`.
//! A pmset value is a number, or `ABSENT` when `pmset -g` does not report the setting
//! (there is nothing to restore); a preference is `0`, `1`, `ABSENT` (`revert` deletes
//! it), or `OTHER`, a value that is not a boolean, which `revert` leaves as it is.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::host::{Host, Mac};
use crate::items::{
    COMMERCE, Ctx, DEFAULTS, Lines, PMSET, SOFTWARE_UPDATE, bool_word, pmset_get, preference_name,
    read_preference,
};

pub const RECORD_DIR: &str = "/var/db/kbf-mac-provision";
pub const RECORD: &str = "/var/db/kbf-mac-provision/settings.before";

/// A setting the record holds, with the item that sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setting {
    /// A `pmset` setting.
    Power(&'static str, &'static str),
    /// A preference: domain (a path on the Mac) and key.
    Preference(&'static str, &'static str, &'static str),
}

use Setting::{Power, Preference};

/// Every setting the record holds, in its order.
const SETTINGS: &[Setting] = &[
    Power("power_sleep", "sleep"),
    Power("power_standby", "standby"),
    Power("power_powernap", "powernap"),
    Power("power_autorestart", "autorestart"),
    Power("power_womp", "womp"),
    Preference(
        "updates_auto_download",
        SOFTWARE_UPDATE,
        "AutomaticDownload",
    ),
    Preference(
        "updates_auto_install_macos",
        SOFTWARE_UPDATE,
        "AutomaticallyInstallMacOSUpdates",
    ),
    Preference(
        "updates_auto_install_security",
        SOFTWARE_UPDATE,
        "CriticalUpdateInstall",
    ),
    Preference("updates_config_data", SOFTWARE_UPDATE, "ConfigDataInstall"),
    Preference(
        "appstore_auto_update",
        SOFTWARE_UPDATE,
        "AutomaticallyInstallAppUpdates",
    ),
    Preference("appstore_auto_update", COMMERCE, "AutoUpdate"),
];

impl Setting {
    fn kind_domain_key(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Power(_, key) => ("pmset", "-", key),
            Preference(_, domain, key) => ("pref", domain, key),
        }
    }

    /// Its value now, in the record's form.
    fn read(self, mac: Mac<'_>) -> String {
        match self {
            Power(_, key) => {
                let v = pmset_get(mac, key);
                if crate::pattern::number(&v) {
                    v
                } else {
                    "ABSENT".into()
                }
            }
            Preference(_, domain, key) => match read_preference(mac, domain, key).as_str() {
                "" => "ABSENT".into(),
                v @ ("0" | "1") => v.into(),
                _ => "OTHER".into(),
            },
        }
    }

    /// Whether `value` is one the record may hold for this setting.
    fn valid(self, value: &str) -> bool {
        match self {
            Power(..) => value == "ABSENT" || crate::pattern::number(value),
            Preference(..) => matches!(value, "ABSENT" | "0" | "1" | "OTHER"),
        }
    }

    /// Puts `value` back, if the setting holds another.
    fn restore(self, mac: Mac<'_>, value: &str, out: &mut Lines) {
        if self.read(mac) == value {
            return;
        }
        match (self, value) {
            (Power(..), "ABSENT") => {}
            (Power(item, key), _) => {
                mac.succeeds(PMSET, &["-a", key, value]);
                out.change(item, format!("pmset -a {key} {value} (reverted)"));
            }
            (Preference(item, domain, key), "OTHER") => out.note(
                item,
                format!(
                    "{} held a value that is not 0 or 1; left as it is",
                    preference_name(domain, key)
                ),
            ),
            (Preference(item, domain, key), "ABSENT") => {
                mac.succeeds(DEFAULTS, &["delete", &mac.path(domain), key]);
                out.change(
                    item,
                    format!("{} deleted (reverted)", preference_name(domain, key)),
                );
            }
            (Preference(item, domain, key), _) => {
                mac.succeeds(
                    DEFAULTS,
                    &["write", &mac.path(domain), key, "-bool", bool_word(value)],
                );
                out.change(
                    item,
                    format!("{} = {value} (reverted)", preference_name(domain, key)),
                );
            }
        }
    }
}

/// Writes the record of every setting as it is now, unless one is already there. Each
/// fix of a recorded setting calls it before it changes anything.
///
/// # Errors
/// The record cannot be written; the fix then changes nothing.
pub(crate) fn keep_before(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    if c.mac.lstat(RECORD).is_some() {
        return Ok(());
    }
    let text: String = SETTINGS
        .iter()
        .map(|s| {
            let (kind, domain, key) = s.kind_domain_key();
            format!("{kind}\t{domain}\t{key}\t{}\n", s.read(c.mac))
        })
        .collect();
    c.mac.dir(RECORD_DIR, 0, 0, 0o700)?;
    c.mac.replace(RECORD, text.as_bytes(), 0, 0, 0o600)?;
    out.change(c.key, format!("wrote {RECORD} (what revert restores)"));
    Ok(())
}

/// Why `revert` changed nothing.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RevertError {
    #[error("no record at {RECORD}: apply never changed a recorded setting, nothing to revert")]
    NoRecord,
    #[error("{RECORD} is {0}, want a regular file of root's that only root may write")]
    NotRoots(String),
    #[error("{RECORD} line {line}: {why}")]
    Malformed { line: usize, why: String },
    #[error("cannot move the record aside: {0}")]
    Move(String),
}

/// The record's lines, each with its setting; refuses the whole record if one line is
/// not a recorded setting with a value the record may hold, or a setting is twice.
fn parse(text: &str) -> Result<Vec<(Setting, String)>, RevertError> {
    let mut parsed: Vec<(Setting, String)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let malformed = |why: String| RevertError::Malformed { line: n + 1, why };
        let fields: Vec<&str> = line.split('\t').collect();
        let [kind, domain, key, value] = fields[..] else {
            return Err(malformed("not four tab-separated fields".into()));
        };
        let setting = SETTINGS
            .iter()
            .copied()
            .find(|s| s.kind_domain_key() == (kind, domain, key))
            .ok_or_else(|| {
                malformed(format!("'{kind} {domain} {key}' is not a recorded setting"))
            })?;
        if !setting.valid(value) {
            return Err(malformed(format!(
                "{key}={value} is not a value revert restores"
            )));
        }
        if parsed.iter().any(|(s, _)| *s == setting) {
            return Err(malformed(format!("{key} is recorded twice")));
        }
        parsed.push((setting, value.to_owned()));
    }
    Ok(parsed)
}

/// `revert`: puts back each recorded setting that differs from its record, then moves
/// the record to `<RECORD>.reverted-<seconds since 1970>`. What it prints.
///
/// # Errors
/// There is no record, it is not root's, or a line is malformed: nothing is changed.
/// Or the record could not be moved aside after the settings were restored.
pub fn revert(host: &dyn Host) -> Result<String, RevertError> {
    let mac = Mac::new(host);
    let stat = mac.lstat(RECORD).ok_or(RevertError::NoRecord)?;
    if stat.kind != '-' || stat.uid != 0 || stat.mode & 0o022 != 0 {
        return Err(RevertError::NotRoots(stat.ls()));
    }
    let bytes = mac.read(RECORD).unwrap_or_default();
    let settings = parse(&String::from_utf8_lossy(&bytes))?;
    let mut out = Lines::default();
    for (setting, value) in &settings {
        setting.restore(mac, value, &mut out);
    }
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = format!("{RECORD}.reverted-{seconds}");
    mac.rename(RECORD, &aside)
        .map_err(|e| RevertError::Move(e.to_string()))?;
    out.change("record", format!("moved {RECORD} to {aside}"));
    Ok(out.into_text())
}
