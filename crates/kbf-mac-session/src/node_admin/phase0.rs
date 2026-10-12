//! `phase0 status|apply|revert`: macOS and App Store updates no longer install on
//! their own, and the Mac powers back on after a power cut and never sleeps.
//!
//! An OS, Xcode or Command Line Tools update changes the Mac's build host identity
//! (its OS build, compiler and linker), so updates must happen when the farm chooses,
//! never on their own. What `apply` sets:
//!
//! | setting | value |
//! |---|---|
//! | `com.apple.SoftwareUpdate` `AutomaticDownload` | false |
//! | `com.apple.SoftwareUpdate` `AutomaticallyInstallMacOSUpdates` | false |
//! | `com.apple.SoftwareUpdate` `CriticalUpdateInstall` | false |
//! | `com.apple.commerce` `AutoUpdate` (App Store apps, Xcode among them) | false |
//! | `pmset -a autorestart` | 1 |
//! | `pmset -a sleep` | 0 |
//!
//! Left alone, and shown by `status`: `ConfigDataInstall` (XProtect and other data
//! files, which do not change the OS build) and `AutomaticCheckEnabled` (checking
//! only).
//!
//! The first `apply` records what it found in `state/phase0.before`, root's, mode
//! 0600, one line per setting (`pref<TAB><domain><TAB><key><TAB><value>`, `ABSENT` for
//! a preference that is not set; `pmset<TAB>-<TAB><key><TAB><value>`), before it
//! changes anything. A later `apply` keeps that record, so `revert` always restores
//! the Mac's original values. `revert` checks the whole record first (each setting
//! exactly once, each value one `revert` can write back), restores each setting that
//! differs, and then moves the record aside to `phase0.before.reverted-<UTC time>`;
//! a failed step keeps the record.
//!
//! An earlier phase-0 tool kept the same record at
//! `/Library/Application Support/kbf-mdm/phase0.before`. When that record exists and
//! this one does not, `apply` checks it like `revert` does and takes it over (its
//! values are the Mac's originals; the values now are the ones it set), then moves it
//! aside to `phase0.before.adopted-<UTC time>`. When both exist, `apply` refuses: it
//! cannot tell which holds the originals.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::{
    AdminHost, Paths, Stop, at, change, meta, query, root_dir, root_file, say, utc, write_new,
};

const DEFAULTS: &str = "/usr/bin/defaults";
const PMSET: &str = "/usr/bin/pmset";
const SW_VERS: &str = "/usr/bin/sw_vers";

const SOFTWARE_UPDATE: &str = "/Library/Preferences/com.apple.SoftwareUpdate";
const COMMERCE: &str = "/Library/Preferences/com.apple.commerce";

/// What `defaults read` failing means, in the record and in `status`.
const ABSENT: &str = "ABSENT";

/// Where a setting lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Store {
    /// A system-wide preference: its `defaults` domain.
    Pref(&'static str),
    /// A `pmset` setting.
    Pmset,
}

/// One setting `apply` may change, and the value it sets.
#[derive(Clone, Copy, Debug)]
struct Setting {
    store: Store,
    key: &'static str,
    want: &'static str,
}

macro_rules! setting {
    (pref $domain:expr, $key:literal, $want:literal) => {
        Setting {
            store: Store::Pref($domain),
            key: $key,
            want: $want,
        }
    };
    (pmset $key:literal, $want:literal) => {
        Setting {
            store: Store::Pmset,
            key: $key,
            want: $want,
        }
    };
}

/// Every setting `apply` may change, in the record's order.
const PLAN: [Setting; 6] = [
    setting!(pref SOFTWARE_UPDATE, "AutomaticDownload", "0"),
    setting!(pref SOFTWARE_UPDATE, "AutomaticallyInstallMacOSUpdates", "0"),
    setting!(pref SOFTWARE_UPDATE, "CriticalUpdateInstall", "0"),
    setting!(pref COMMERCE, "AutoUpdate", "0"),
    setting!(pmset "autorestart", "1"),
    setting!(pmset "sleep", "0"),
];

/// Preferences `status` shows and nothing changes.
const KEPT: [&str; 2] = ["ConfigDataInstall", "AutomaticCheckEnabled"];

/// The folder of an earlier phase-0 tool's record, under the root.
fn earlier_dir(paths: &Paths) -> PathBuf {
    paths.root.join("Library/Application Support/kbf-mdm")
}

fn earlier_record(paths: &Paths) -> PathBuf {
    earlier_dir(paths).join("phase0.before")
}

fn record_path(paths: &Paths) -> PathBuf {
    paths.state.join("phase0.before")
}

impl Setting {
    fn domain(self) -> &'static str {
        match self.store {
            Store::Pref(domain) => domain,
            Store::Pmset => "-",
        }
    }

    fn kind(self) -> &'static str {
        match self.store {
            Store::Pref(_) => "pref",
            Store::Pmset => "pmset",
        }
    }

    /// How `status` names it.
    fn label(self) -> String {
        match self.store {
            Store::Pref(domain) => format!("{} {}", base_name(domain), self.key),
            Store::Pmset => format!("pmset {}", self.key),
        }
    }

    /// Its value now.
    fn get(self, host: &dyn AdminHost) -> Result<String, Stop> {
        match self.store {
            Store::Pref(domain) => pref(host, domain, self.key),
            Store::Pmset => pmset(host, self.key),
        }
    }

    /// Sets it to `value` (`ABSENT` deletes a preference).
    fn set(self, host: &dyn AdminHost, out: &mut dyn Write, value: &str) -> Result<(), Stop> {
        match (self.store, value) {
            (Store::Pmset, _) => change(host, out, PMSET, &["-a", self.key, value]),
            (Store::Pref(domain), ABSENT) => {
                change(host, out, DEFAULTS, &["delete", domain, self.key])
            }
            (Store::Pref(domain), _) => {
                let value = if value == "1" { "true" } else { "false" };
                change(
                    host,
                    out,
                    DEFAULTS,
                    &["write", domain, self.key, "-bool", value],
                )
            }
        }
    }

    /// Whether `value` is one `revert` writes back for this setting.
    fn restorable(self, value: &str) -> bool {
        match self.store {
            Store::Pref(_) => matches!(value, ABSENT | "0" | "1"),
            Store::Pmset => {
                (1..=3).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
            }
        }
    }
}

fn base_name(domain: &str) -> &str {
    domain.rsplit('/').next().unwrap_or(domain)
}

/// A preference's value: what `defaults read` prints, `ABSENT` when it fails (the
/// key is not set).
fn pref(host: &dyn AdminHost, domain: &str, key: &str) -> Result<String, Stop> {
    let output = query(host, DEFAULTS, &["read", domain, key])?;
    Ok(if output.success {
        output.stdout.trim().to_owned()
    } else {
        ABSENT.to_owned()
    })
}

/// A `pmset` setting's value: the second word of the line of `pmset -g` whose first
/// word is `key`.
fn pmset(host: &dyn AdminHost, key: &str) -> Result<String, Stop> {
    let output = query(host, PMSET, &["-g"])?;
    if !output.success {
        return Err(Stop::Failed(format!(
            "pmset -g failed: {}",
            output.stderr.trim()
        )));
    }
    let found = output.stdout.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some(key)).then(|| words.next().unwrap_or_default().to_owned())
    });
    found.ok_or_else(|| Stop::Failed(format!("pmset -g lists no {key}")))
}

fn sw_vers(host: &dyn AdminHost, flag: &str) -> String {
    host.command(SW_VERS, &[flag])
        .ok()
        .filter(|output| output.success)
        .map_or_else(|| "?".to_owned(), |output| output.stdout.trim().to_owned())
}

fn show(host: &dyn AdminHost, out: &mut dyn Write, title: &str) -> Result<(), Stop> {
    say(out, title);
    for setting in PLAN {
        let now = setting.get(host)?;
        let label = setting.label();
        say(out, &format!("  {label:<58} {now} (want {})", setting.want));
    }
    for key in KEPT {
        let now = pref(host, SOFTWARE_UPDATE, key)?;
        let label = format!("com.apple.SoftwareUpdate {key} (kept)");
        say(out, &format!("  {label:<58} {now}"));
    }
    Ok(())
}

/// `phase0 status`.
///
/// # Errors
/// A setting could not be read.
pub(crate) fn status(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write) -> Result<(), Stop> {
    let version = sw_vers(host, "-productVersion");
    let build = sw_vers(host, "-buildVersion");
    say(out, &format!("Mac: {version} ({build})"));
    show(host, out, "phase0:")?;
    let record = record_path(paths);
    if meta(host, &record)?.is_some() {
        say(
            out,
            &format!("record: {} (phase0 revert restores it)", record.display()),
        );
    } else {
        say(out, "record: none (phase0 apply never ran)");
    }
    Ok(())
}

/// The record's text for `values`, in [`PLAN`]'s order.
fn render(values: &[String]) -> String {
    PLAN.iter()
        .zip(values)
        .map(|(setting, value)| {
            format!(
                "{}\t{}\t{}\t{value}\n",
                setting.kind(),
                setting.domain(),
                setting.key
            )
        })
        .collect()
}

/// The values a record holds, in [`PLAN`]'s order: every setting exactly once, each
/// value one `revert` writes back.
fn parse(path: &Path, text: &str) -> Result<Vec<String>, Stop> {
    let refused = |why: String| Stop::Refused(format!("{}: {why}", path.display()));
    let mut values: Vec<Option<String>> = vec![None; PLAN.len()];
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [kind, domain, key, value] = fields[..] else {
            return Err(refused(format!(
                "{line:?} is not kind<TAB>domain<TAB>key<TAB>value"
            )));
        };
        let Some(at) = PLAN
            .iter()
            .position(|s| s.kind() == kind && s.domain() == domain && s.key == key)
        else {
            return Err(refused(format!(
                "'{kind} {domain} {key}' is not a phase0 setting"
            )));
        };
        if !PLAN[at].restorable(value) {
            return Err(refused(format!(
                "{key}={value} is not a value phase0 restores"
            )));
        }
        if values[at].replace(value.to_owned()).is_some() {
            return Err(refused(format!("{key} is listed twice")));
        }
    }
    values
        .into_iter()
        .zip(PLAN)
        .map(|(value, setting)| {
            value.ok_or_else(|| refused(format!("{} is not in the record", setting.key)))
        })
        .collect()
}

/// Reads and checks the record at `path`, which must be root's.
fn read_record(host: &dyn AdminHost, path: &Path) -> Result<Vec<String>, Stop> {
    root_file(host, path)?;
    let text = at(path, fs::read_to_string(path))?;
    parse(path, &text)
}

/// `phase0 apply`.
///
/// # Errors
/// Refused (a record that is not root's, two records) or a step failed.
pub(crate) fn apply(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write) -> Result<(), Stop> {
    let record = record_path(paths);
    let earlier = earlier_record(paths);
    let kept = meta(host, &record)?.is_some();
    let adopted = meta(host, &earlier)?.is_some();
    if adopted {
        if kept {
            return Err(Stop::Refused(format!(
                "both {} and {} exist; it is not clear which holds the Mac's original values",
                record.display(),
                earlier.display()
            )));
        }
        adopt(host, paths, &earlier, &record, out)?;
    }
    show(host, out, "before:")?;
    if kept {
        root_file(host, &record)?;
        say(
            out,
            &format!(
                "record: {} kept (it holds what the first apply found)",
                record.display()
            ),
        );
    } else if !adopted {
        let mut values = Vec::with_capacity(PLAN.len());
        for setting in PLAN {
            values.push(setting.get(host)?);
        }
        write_new(&paths.state, &record, &render(&values))?;
        say(out, &format!("record: wrote {}", record.display()));
    }
    for setting in PLAN {
        if setting.get(host)? != setting.want {
            setting.set(host, out, setting.want)?;
        }
    }
    show(host, out, "after:")
}

/// Takes over the earlier tool's record: checked, copied into `record`, moved aside.
fn adopt(
    host: &dyn AdminHost,
    paths: &Paths,
    earlier: &Path,
    record: &Path,
    out: &mut dyn Write,
) -> Result<(), Stop> {
    root_dir(host, &earlier_dir(paths))?;
    let values = read_record(host, earlier)?;
    write_new(&paths.state, record, &render(&values))?;
    let aside = aside(earlier, "adopted", host);
    at(earlier, fs::rename(earlier, &aside))?;
    say(
        out,
        &format!(
            "record: took over {} (the Mac's original values) as {}; moved it to {}",
            earlier.display(),
            record.display(),
            aside.display()
        ),
    );
    Ok(())
}

fn aside(path: &Path, what: &str, host: &dyn AdminHost) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{what}-{}", utc(host.now(), true)));
    PathBuf::from(name)
}

/// `phase0 revert`.
///
/// # Errors
/// Refused (no record, a record that is not root's or not valid) or a step failed.
pub(crate) fn revert(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write) -> Result<(), Stop> {
    let record = record_path(paths);
    if meta(host, &record)?.is_none() {
        return Err(Stop::Refused(format!(
            "no record at {}: phase0 apply never ran, nothing to revert",
            record.display()
        )));
    }
    let values = read_record(host, &record)?;
    for (setting, value) in PLAN.into_iter().zip(&values) {
        if setting.get(host)? != *value {
            setting.set(host, out, value)?;
        }
    }
    let aside = aside(&record, "reverted", host);
    at(&record, fs::rename(&record, &aside))?;
    say(out, &format!("record: moved to {}", aside.display()));
    show(host, out, "after:")
}
