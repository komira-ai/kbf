//! The record of the power and software update settings as `apply` first found them,
//! and `revert`, which puts them back.
//!
//! Before the first change to any of these settings, `apply` writes every one of them
//! (not only the one it changes) to [`RECORD`], root's, mode 600. Later runs keep that
//! file, so it holds the values from before kbf changed anything, however many applies
//! ran since. `revert` checks the whole record (it must hold every setting), restores
//! each setting that differs, reads each one back, and moves the record aside, so the
//! next `apply` records afresh. If a setting is not put back, the record stays, and
//! `revert` can be run again.
//!
//! A line is `<kind>\t<domain>\t<key>\t<value>`:
//! `pmset\t-\tsleep\t1` or
//! `pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticDownload\tABSENT`.
//! A pmset value is a number, or `ABSENT` when `pmset -g` does not report the setting
//! (there is nothing to restore); a preference is `0`, `1`, `ABSENT` (`revert` deletes
//! it), or `OTHER`, a value that is not a boolean, which `revert` leaves as it is.

use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::host::{Host, Mac};
use crate::items::{
    COMMERCE, Ctx, DEFAULTS, Lines, PMSET, Report, SOFTWARE_UPDATE, bool_word, pmset_get,
    preference_name, read_preference,
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

    /// The item that sets it, and its name in what `revert` prints.
    fn item_and_name(self) -> (&'static str, String) {
        match self {
            Power(item, key) => (item, key.to_owned()),
            Preference(item, domain, key) => (item, preference_name(domain, key)),
        }
    }

    /// Runs the command that puts `value` back, if the setting holds another; `None`
    /// if it holds `value`, or `value` says pmset did not report it.
    fn restore(self, mac: Mac<'_>, value: &str) -> Option<Restore> {
        if self.read(mac) == value {
            return None;
        }
        match (self, value) {
            (Power(..), "ABSENT") => None,
            (Power(_, key), _) => Some(Restore::Ran {
                change: format!("pmset -a {key} {value}"),
                command: format!("pmset -a {key} {value}"),
                ok: mac.succeeds(PMSET, &["-a", key, value]),
            }),
            (Preference(..), "OTHER") => Some(Restore::Left),
            (Preference(_, domain, key), "ABSENT") => Some(Restore::Ran {
                change: format!("{} deleted", preference_name(domain, key)),
                command: format!("defaults delete {domain} {key}"),
                ok: mac.succeeds(DEFAULTS, &["delete", &mac.path(domain), key]),
            }),
            (Preference(_, domain, key), _) => {
                let word = bool_word(value);
                Some(Restore::Ran {
                    change: format!("{} = {value}", preference_name(domain, key)),
                    command: format!("defaults write {domain} {key} -bool {word}"),
                    ok: mac.succeeds(DEFAULTS, &["write", &mac.path(domain), key, "-bool", word]),
                })
            }
        }
    }
}

/// What `revert` did about a setting that differs from its record.
enum Restore {
    /// It ran a command to put it back.
    Ran {
        /// What its `CHANGE` line says once the setting reads back as recorded.
        change: String,
        /// The command as a `FAIL` line shows it, with paths on the Mac.
        command: String,
        /// Whether it exited 0.
        ok: bool,
    },
    /// The record holds a value that is not a boolean, which it cannot put back.
    Left,
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

/// Why `revert` refused the record (it then changed nothing), or could not move it
/// aside after it put every setting back.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RevertError {
    #[error("no record at {RECORD}: apply never changed a recorded setting, nothing to revert")]
    NoRecord,
    #[error("cannot read {RECORD}: {0}")]
    Unreadable(String),
    #[error("{RECORD} is empty: no setting to revert, so it stays")]
    Empty,
    #[error("{RECORD} is {0}, want a regular file of root's that only root may write")]
    NotRoots(String),
    #[error("{RECORD} line {line}: {why}")]
    Malformed { line: usize, why: String },
    /// The record lacks these settings (named as `revert` prints them).
    #[error("{RECORD} does not hold {0}: revert restores every recorded setting or none")]
    Incomplete(String),
    #[error("cannot move the record aside: {0}")]
    Move(String),
}

/// The record's lines, each with its setting; refuses the whole record if one line is
/// not a recorded setting with a value the record may hold, a setting is twice, or
/// one is missing (an empty record lacks them all).
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
    if parsed.is_empty() {
        return Err(RevertError::Empty);
    }
    let missing: Vec<String> = SETTINGS
        .iter()
        .filter(|s| !parsed.iter().any(|(p, _)| p == *s))
        .map(|s| s.item_and_name().1)
        .collect();
    if !missing.is_empty() {
        return Err(RevertError::Incomplete(missing.join(", ")));
    }
    Ok(parsed)
}

/// `revert`: puts back each recorded setting that differs from its record, reads each
/// one it put back again, then moves the record to `<RECORD>.reverted-<seconds since
/// 1970>`. A setting whose command failed, or that does not read back as recorded, gets
/// a `FAIL` line and counts as failed; the record then stays where it is, as it was, so
/// a later revert can finish. What it prints, and how many settings failed.
///
/// # Errors
/// There is no record, it is not root's, it cannot be read, a line is malformed, or a
/// setting is missing: nothing is changed. Or the record could not be moved aside after
/// every setting was put back.
pub fn revert(host: &dyn Host) -> Result<Report, RevertError> {
    let mac = Mac::new(host);
    let stat = mac.lstat(RECORD).ok_or(RevertError::NoRecord)?;
    if stat.kind != '-' || stat.uid != 0 || stat.mode & 0o022 != 0 {
        return Err(RevertError::NotRoots(stat.ls()));
    }
    let bytes = fs::read(mac.path(RECORD)).map_err(|e| RevertError::Unreadable(e.to_string()))?;
    let settings = parse(&String::from_utf8_lossy(&bytes))?;
    // Every command runs before any setting is read back, so each read sees the Mac
    // after all of them.
    let restored: Vec<(Setting, &str, Restore)> = settings
        .iter()
        .filter_map(|(setting, value)| {
            Some((*setting, value.as_str(), setting.restore(mac, value)?))
        })
        .collect();
    let mut out = Lines::default();
    let mut failed = 0;
    for (setting, value, restore) in restored {
        let (item, name) = setting.item_and_name();
        let Restore::Ran {
            change,
            command,
            ok,
        } = restore
        else {
            out.note(
                item,
                format!("{name} held a value that is not 0 or 1; left as it is"),
            );
            continue;
        };
        let now = setting.read(mac);
        if ok && now == value {
            out.change(item, format!("{change} (reverted)"));
            continue;
        }
        failed += 1;
        out.fail(
            item,
            if ok {
                format!("{name} is {now} after {command}, want {value}")
            } else {
                format!("{command} failed; {name} is {now}, want {value}")
            },
        );
    }
    if failed != 0 {
        let (are, they) = if failed == 1 {
            ("setting is", "it")
        } else {
            ("settings are", "they")
        };
        out.note(
            "record",
            format!("kept {RECORD}: {failed} {are} not put back; revert again once {they} can be"),
        );
        return Ok(Report {
            text: out.into_text(),
            failed,
        });
    }
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = format!("{RECORD}.reverted-{seconds}");
    mac.rename(RECORD, &aside)
        .map_err(|e| RevertError::Move(e.to_string()))?;
    out.change("record", format!("moved {RECORD} to {aside}"));
    Ok(Report {
        text: out.into_text(),
        failed: 0,
    })
}
