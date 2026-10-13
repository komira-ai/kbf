//! The items: one row per profile key that names a setting of the Mac, each with its
//! check and, unless it is *check only*, its fix. A check prints `PASS <key>: <what it
//! found>` or `FAIL <key>: <why>`; a failing check only item adds that `apply` does not
//! change it (an erase, a restore or the operator is its remedy). A fix prints one
//! `CHANGE <key>: <what>` line per change it makes, and `NOTE <key>: <why>` when it
//! cannot make one.

use std::fmt::Display;
use std::io;

use crate::host::{Host, Mac};
use crate::profile::Profile;
use crate::{accounts, launchd, pool, record, software, sshd};

pub(crate) const SCUTIL: &str = "/usr/sbin/scutil";
pub(crate) const PMSET: &str = "/usr/bin/pmset";
pub(crate) const DEFAULTS: &str = "/usr/bin/defaults";
pub(crate) const SYSTEMSETUP: &str = "/usr/sbin/systemsetup";
pub(crate) const SOCKETFILTERFW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
pub(crate) const FDESETUP: &str = "/usr/bin/fdesetup";
pub(crate) const SNTP: &str = "/usr/bin/sntp";
pub(crate) const SW_VERS: &str = "/usr/bin/sw_vers";

/// The software update preferences.
pub(crate) const SOFTWARE_UPDATE: &str = "/Library/Preferences/com.apple.SoftwareUpdate";
/// The App Store's older preferences, which still hold its `AutoUpdate` switch.
pub(crate) const COMMERCE: &str = "/Library/Preferences/com.apple.commerce";

const VERIFY_27: &str = "the SoftwareUpdate payload is removed in macOS 27: verify the pinned version honours this local preference";

/// A check's verdict and the text after `<key>: `.
pub(crate) struct Outcome {
    pass: bool,
    message: String,
}

pub(crate) fn pass(message: impl Into<String>) -> Outcome {
    Outcome {
        pass: true,
        message: message.into(),
    }
}

pub(crate) fn fail(message: impl Into<String>) -> Outcome {
    Outcome {
        pass: false,
        message: message.into(),
    }
}

/// What a check or a fix reads: the Mac, the profile, and the key of its item.
pub(crate) struct Ctx<'a> {
    pub(crate) mac: Mac<'a>,
    pub(crate) profile: &'a Profile,
    pub(crate) key: &'static str,
}

impl Ctx<'_> {
    /// The profile's value for this item's key.
    pub(crate) fn value(&self) -> &str {
        self.profile.get(self.key)
    }
}

/// The lines a fix prints.
#[derive(Debug, Default)]
pub(crate) struct Lines(String);

impl Lines {
    /// `CHANGE <key>: <what>`: a change made to the Mac.
    pub(crate) fn change(&mut self, key: &str, what: impl Display) {
        self.0.push_str(&format!("CHANGE {key}: {what}\n"));
    }

    /// `NOTE <key>: <why>`: a change the fix could not make.
    pub(crate) fn note(&mut self, key: &str, why: impl Display) {
        self.0.push_str(&format!("NOTE {key}: {why}\n"));
    }

    /// `FAIL <key>: <why>`: a change the verb made that did not take.
    pub(crate) fn fail(&mut self, key: &str, why: impl Display) {
        self.0.push_str(&format!("FAIL {key}: {why}\n"));
    }

    pub(crate) fn into_text(self) -> String {
        self.0
    }
}

/// A fix: makes the Mac what the profile says for one item, saying what it changed. An
/// error stops the item's fix; `apply` prints it as a `NOTE` and checks again.
type Fix = fn(&Ctx<'_>, &mut Lines) -> io::Result<()>;

/// One item: its key, its check, and its fix (none for a check only item).
pub struct Item {
    pub key: &'static str,
    check: fn(&Ctx<'_>) -> Outcome,
    fix: Option<Fix>,
}

impl Item {
    /// `apply` never changes it; a failure says so.
    pub fn check_only(&self) -> bool {
        self.fix.is_none()
    }
}

/// One row of [`ITEMS`]: `row!(key, check, fix)`, or `row!(check only key, check)`.
macro_rules! row {
    ($key:literal, $check:expr, $fix:expr) => {
        Item {
            key: $key,
            check: $check,
            fix: Some($fix),
        }
    };
    (check only $key:literal, $check:expr) => {
        Item {
            key: $key,
            check: $check,
            fix: None,
        }
    };
}

/// Every item, in the order `check` reports them and `apply` fixes them.
pub const ITEMS: &[Item] = &[
    row!(check only "macos_version", macos_version),
    row!(check only "macos_build", macos_build),
    row!("hostname", hostname, fix_hostname),
    row!(check only "admin_user", accounts::admin_user),
    row!("role_user", accounts::role_user, accounts::fix_role_user),
    row!("kbf_volume", pool::kbf_volume, pool::fix_kbf_volume),
    row!("pool_users", pool::pool_users_item, pool::fix_pool_users),
    row!("power_sleep", pmset, fix_pmset),
    row!("power_standby", pmset, fix_pmset),
    row!("power_powernap", pmset, fix_pmset),
    row!("power_autorestart", pmset, fix_pmset),
    row!("power_womp", pmset, fix_pmset),
    row!("updates_auto_download", preferences, fix_preferences),
    row!("updates_auto_install_macos", preferences, fix_preferences),
    row!(
        "updates_auto_install_security",
        preferences,
        fix_preferences
    ),
    row!("updates_config_data", preferences, fix_preferences),
    row!("appstore_auto_update", preferences, fix_preferences),
    row!("remote_login", remote_login, fix_remote_login),
    row!(
        "ssh_password_auth",
        sshd::ssh_password_auth,
        sshd::fix_ssh_password_auth
    ),
    row!("firewall", firewall, fix_firewall),
    row!(check only "filevault", filevault),
    row!("autologin", accounts::autologin, accounts::fix_autologin),
    row!("time_server", time_server, fix_time_server),
    row!(
        "time_max_offset_ms",
        time_max_offset_ms,
        fix_time_max_offset_ms
    ),
    row!("xcode", software::xcode, software::fix_xcode),
    row!(
        "default_developer_dir",
        software::default_developer_dir,
        software::fix_default_developer_dir
    ),
    row!(check only "probe", software::probe),
    row!(check only "simulator_runtimes", software::simulator_runtimes),
    row!(
        "daemon_user",
        launchd::daemon_user,
        launchd::fix_daemon_user
    ),
    row!(
        "launchd_label",
        launchd::launchd_label,
        launchd::fix_launchd_label
    ),
];

/// `--keys` named an item that does not exist.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("--keys: unknown item: {0} (items: {items})", items = ITEMS.iter().map(|i| i.key).collect::<Vec<_>>().join(" "))]
pub struct UnknownItem(pub String);

/// The items `--keys` names (a comma-separated list, in its order), or every item.
///
/// # Errors
/// A name is not an item's key.
pub fn select(keys: Option<&str>) -> Result<Vec<&'static Item>, UnknownItem> {
    let Some(keys) = keys else {
        return Ok(ITEMS.iter().collect());
    };
    keys.split(',')
        .filter(|k| !k.is_empty())
        .map(|k| {
            ITEMS
                .iter()
                .find(|i| i.key == k)
                .ok_or_else(|| UnknownItem(k.to_owned()))
        })
        .collect()
}

/// What a verb printed, and how many items failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub text: String,
    pub failed: usize,
}

/// One line per item, as `check` prints it, and whether the item passed.
fn verdicts(host: &dyn Host, profile: &Profile, items: &[&Item]) -> Vec<(bool, String)> {
    let mac = Mac::new(host);
    items
        .iter()
        .map(|item| {
            let ctx = Ctx {
                mac,
                profile,
                key: item.key,
            };
            let outcome = (item.check)(&ctx);
            let verdict = if outcome.pass { "PASS" } else { "FAIL" };
            let suffix = if !outcome.pass && item.check_only() {
                " [check only: apply does not change this]"
            } else {
                ""
            };
            let line = format!("{verdict} {}: {}{suffix}\n", item.key, outcome.message);
            (outcome.pass, line)
        })
        .collect()
}

/// Checks `items` on the Mac: one line per item, then `check: N pass, M fail`.
pub fn check(host: &dyn Host, profile: &Profile, items: &[&Item]) -> Report {
    let lines = verdicts(host, profile, items);
    let failed = lines.iter().filter(|(passed, _)| !passed).count();
    let mut text: String = lines.into_iter().map(|(_, line)| line).collect();
    text.push_str(&format!(
        "check: {} pass, {failed} fail\n",
        items.len() - failed
    ));
    Report { text, failed }
}

/// Fixes each of `items` that fails and is not check only, in order, then checks them
/// all again: the fixes' `CHANGE` and `NOTE` lines, then a `FAIL` line for each item
/// that still fails. A Mac that already passes gets no change and no line.
pub fn apply(host: &dyn Host, profile: &Profile, items: &[&Item]) -> Report {
    let mac = Mac::new(host);
    let mut lines = Lines::default();
    for item in items {
        let ctx = Ctx {
            mac,
            profile,
            key: item.key,
        };
        let Some(fix) = item.fix else { continue };
        if (item.check)(&ctx).pass {
            continue;
        }
        if let Err(e) = fix(&ctx, &mut lines) {
            lines.note(item.key, e);
        }
    }
    let mut text = lines.into_text();
    let mut failed = 0;
    for (_, line) in verdicts(host, profile, items)
        .into_iter()
        .filter(|(passed, _)| !passed)
    {
        failed += 1;
        text.push_str(&line);
    }
    Report { text, failed }
}

/// Runs `program args` for a change; whether it succeeded shows in the check after.
pub(crate) fn run(c: &Ctx<'_>, program: &str, args: &[&str]) {
    c.mac.succeeds(program, args);
}

/// `sw_vers <flag>` prints the profile's value.
fn sw_vers(c: &Ctx<'_>, flag: &str, what: &str) -> Outcome {
    let found = c.mac.out(SW_VERS, &[flag]);
    if found == c.value() {
        pass(found)
    } else {
        fail(format!(
            "{what} {}, the profile pins {}",
            or(&found, "unknown"),
            c.value()
        ))
    }
}

fn macos_version(c: &Ctx<'_>) -> Outcome {
    sw_vers(c, "-productVersion", "macOS")
}

fn macos_build(c: &Ctx<'_>) -> Outcome {
    sw_vers(c, "-buildVersion", "build")
}

/// `value`, or `instead` when it is empty.
pub(crate) fn or<'a>(value: &'a str, instead: &'a str) -> &'a str {
    if value.is_empty() { instead } else { value }
}

const HOST_NAMES: [&str; 3] = ["HostName", "LocalHostName", "ComputerName"];

/// All three names `scutil` keeps are the profile's.
fn hostname(c: &Ctx<'_>) -> Outcome {
    let want = c.value();
    let wrong: String = HOST_NAMES
        .into_iter()
        .filter_map(|name| {
            let found = c.mac.out(SCUTIL, &["--get", name]);
            (found != want).then(|| format!(" {name}={}", or(&found, "unset")))
        })
        .collect();
    if wrong.is_empty() {
        pass(want)
    } else {
        fail(format!("want {want}, found{wrong}"))
    }
}

/// Sets each of the three names that is not the profile's.
fn fix_hostname(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let want = c.value();
    for name in HOST_NAMES {
        if c.mac.out(SCUTIL, &["--get", name]) != want {
            run(c, SCUTIL, &["--set", name, want]);
            out.change(c.key, format!("{name} = {want}"));
        }
    }
    Ok(())
}

/// The value `pmset -g` reports for `setting`, or empty. The name matches exactly:
/// `Sleep On Power Button` is not `sleep`.
pub(crate) fn pmset_get(mac: Mac<'_>, setting: &str) -> String {
    mac.out(PMSET, &["-g"])
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(setting)).then(|| words.next().unwrap_or_default().to_owned())
        })
        .unwrap_or_default()
}

/// `pmset -g` reports the setting (named by the key after `power_`) at the profile's
/// value.
fn pmset(c: &Ctx<'_>) -> Outcome {
    let setting = c.key.trim_start_matches("power_");
    let found = pmset_get(c.mac, setting);
    if found == c.value() {
        pass(format!("{setting} {found}"))
    } else {
        fail(format!(
            "{setting} is {}, want {}",
            or(&found, "not reported"),
            c.value()
        ))
    }
}

/// Keeps the record of the values before, then `pmset -a <setting> <value>`.
fn fix_pmset(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    record::keep_before(c, out)?;
    let setting = c.key.trim_start_matches("power_");
    run(c, PMSET, &["-a", setting, c.value()]);
    out.change(c.key, format!("pmset -a {setting} {}", c.value()));
    Ok(())
}

/// The preferences (domain, key) an update item sets, each to the profile's value.
pub(crate) fn preference_keys(key: &str) -> &'static [(&'static str, &'static str)] {
    match key {
        "updates_auto_download" => &[(SOFTWARE_UPDATE, "AutomaticDownload")],
        "updates_auto_install_macos" => &[(SOFTWARE_UPDATE, "AutomaticallyInstallMacOSUpdates")],
        "updates_auto_install_security" => &[(SOFTWARE_UPDATE, "CriticalUpdateInstall")],
        "updates_config_data" => &[(SOFTWARE_UPDATE, "ConfigDataInstall")],
        _ => &[
            (SOFTWARE_UPDATE, "AutomaticallyInstallAppUpdates"),
            (COMMERCE, "AutoUpdate"),
        ],
    }
}

/// How a preference is named in a line: its key, with the domain's name before it when
/// that is not com.apple.SoftwareUpdate.
pub(crate) fn preference_name(domain: &str, key: &str) -> String {
    if domain == SOFTWARE_UPDATE {
        key.to_owned()
    } else {
        format!("{} {key}", domain.rsplit('/').next().unwrap_or(domain))
    }
}

/// `defaults read` of `domain` (a path on the Mac) and `key`, or empty.
pub(crate) fn read_preference(mac: Mac<'_>, domain: &str, key: &str) -> String {
    mac.out(DEFAULTS, &["read", &mac.path(domain), key])
}

/// `defaults read` of `domain` (a path on the Mac) and `key`, or empty.
pub(crate) fn defaults_read(c: &Ctx<'_>, domain: &str, key: &str) -> String {
    read_preference(c.mac, domain, key)
}

/// Every preference of the item holds the profile's value.
fn preferences(c: &Ctx<'_>) -> Outcome {
    let found: Vec<(String, String)> = preference_keys(c.key)
        .iter()
        .map(|(domain, key)| (preference_name(domain, key), defaults_read(c, domain, key)))
        .collect();
    if found.iter().all(|(_, v)| v == c.value()) {
        let all: Vec<String> = found.iter().map(|(n, v)| format!("{n} = {v}")).collect();
        pass(format!("{} ({VERIFY_27})", all.join(", ")))
    } else {
        let wrong: Vec<String> = found
            .iter()
            .filter(|(_, v)| v != c.value())
            .map(|(n, v)| format!("{n} is {}", or(v, "unset")))
            .collect();
        fail(format!(
            "{}, want {} ({VERIFY_27})",
            wrong.join(", "),
            c.value()
        ))
    }
}

/// `true` or `false` for a profile bit, as `defaults write -bool` takes it.
pub(crate) fn bool_word(bit: &str) -> &'static str {
    if bit == "1" { "true" } else { "false" }
}

/// Keeps the record of the values before, then writes each preference that differs.
fn fix_preferences(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    record::keep_before(c, out)?;
    for (domain, key) in preference_keys(c.key) {
        if defaults_read(c, domain, key) != c.value() {
            let path = c.mac.path(domain);
            run(
                c,
                DEFAULTS,
                &["write", &path, key, "-bool", bool_word(c.value())],
            );
            out.change(
                c.key,
                format!("{} = {}", preference_name(domain, key), c.value()),
            );
        }
    }
    Ok(())
}

fn on_off(bit: &str) -> &'static str {
    if bit == "1" { "On" } else { "Off" }
}

fn remote_login(c: &Ctx<'_>) -> Outcome {
    let found = c.mac.out(SYSTEMSETUP, &["-getremotelogin"]);
    let want = format!("Remote Login: {}", on_off(c.value()));
    if found == want {
        pass(found)
    } else {
        fail(format!("want {want}, found {}", or(&found, "nothing")))
    }
}

fn fix_remote_login(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let state = on_off(c.value()).to_ascii_lowercase();
    run(c, SYSTEMSETUP, &["-f", "-setremotelogin", &state]);
    out.change(c.key, format!("Remote Login {state}"));
    Ok(())
}

fn firewall(c: &Ctx<'_>) -> Outcome {
    let found = c.mac.out(SOCKETFILTERFW, &["--getglobalstate"]);
    let state = if found.contains("is enabled") {
        "on"
    } else if found.contains("is disabled") {
        "off"
    } else {
        "unknown"
    };
    if state == c.value() {
        pass(state)
    } else {
        fail(format!("firewall {state}, want {}", c.value()))
    }
}

fn fix_firewall(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    run(c, SOCKETFILTERFW, &["--setglobalstate", c.value()]);
    out.change(c.key, format!("firewall {}", c.value()));
    Ok(())
}

fn filevault(c: &Ctx<'_>) -> Outcome {
    let found = c.mac.out(FDESETUP, &["status"]);
    let first = found.lines().next().unwrap_or_default();
    let state = match first {
        "FileVault is On." => "on".to_owned(),
        "FileVault is Off." => "off".to_owned(),
        _ => format!("unknown ({})", or(first, "no answer")),
    };
    if state == c.value() {
        pass(state)
    } else {
        fail(format!("FileVault {state}, want {}", c.value()))
    }
}

fn time_server(c: &Ctx<'_>) -> Outcome {
    let on = c.mac.out(SYSTEMSETUP, &["-getusingnetworktime"]);
    let server = c.mac.out(SYSTEMSETUP, &["-getnetworktimeserver"]);
    let want = c.value();
    if on != "Network Time: On" {
        fail(format!("network time is not on ({})", or(&on, "no answer")))
    } else if server != format!("Network Time Server: {want}") {
        fail(format!("want {want}, found {}", or(&server, "no answer")))
    } else {
        pass(want)
    }
}

fn fix_time_server(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let server = c.value();
    run(c, SYSTEMSETUP, &["-setusingnetworktime", "on"]);
    run(c, SYSTEMSETUP, &["-setnetworktimeserver", server]);
    out.change(c.key, format!("network time on, server {server}"));
    Ok(())
}

/// The size of an offset `sntp` prints (`+0.004000`, `-1.5`), in whole milliseconds,
/// truncated; `None` if `word` is not `[+-]?[0-9]+(\.[0-9]+)?`.
pub fn offset_ms(word: &str) -> Option<u64> {
    let unsigned = word.strip_prefix(['+', '-']).unwrap_or(word);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, "0"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !digits(fraction) {
        return None;
    }
    let millis: String = fraction.chars().chain("000".chars()).take(3).collect();
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(1000)?
        .checked_add(millis.parse::<u64>().ok()?)
}

/// The clock is within `time_max_offset_ms` of the time server, by `sntp -t 5`
/// (which only reads).
fn time_max_offset_ms(c: &Ctx<'_>) -> Outcome {
    let server = c.profile.get("time_server");
    let max = c.profile.time_max_offset_ms;
    let answer = c.mac.out(SNTP, &["-t", "5", server]);
    let offset = answer
        .lines()
        .find_map(|line| offset_ms(line.split_whitespace().next().unwrap_or_default()));
    match offset {
        None => fail(format!("no answer from {server}")),
        Some(ms) if ms <= max => pass(format!("{ms} ms from {server}")),
        Some(ms) => fail(format!("{ms} ms from {server}, more than {max}")),
    }
}

/// Sets the clock from the time server (`sntp -sS`).
fn fix_time_max_offset_ms(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let server = c.profile.get("time_server");
    run(c, SNTP, &["-sS", server]);
    out.change(c.key, format!("clock set from {server}"));
    Ok(())
}
