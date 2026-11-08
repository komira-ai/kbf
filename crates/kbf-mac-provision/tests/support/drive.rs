//! The verbs the tests drive on a fake Mac, and what they read back from it.

use kbf_mac_provision::items::Report;

use super::FakeMac;

pub fn check_keys(mac: &FakeMac, keys: Option<&str>) -> Report {
    let path = mac.profile_file(&mac.profile_text());
    kbf_mac_provision::cli::check(&path, keys, mac).unwrap()
}

pub fn check(mac: &FakeMac) -> Report {
    check_keys(mac, None)
}

/// The lines of a report that start with `prefix`.
pub fn lines<'a>(r: &'a Report, prefix: &str) -> Vec<&'a str> {
    r.text.lines().filter(|l| l.starts_with(prefix)).collect()
}

/// On a converged Mac changed by `drift`, check fails `item` and nothing else, and
/// says `text`. `Err` says what went wrong instead.
pub fn only_fails(
    name: &str,
    item: &str,
    drift: fn(&mut FakeMac),
    text: &str,
) -> Result<(), String> {
    let mut mac = FakeMac::converged(name);
    drift(&mut mac);
    let r = check(&mac);
    let failed = lines(&r, "FAIL");
    let ok = r.failed == 1
        && failed.len() == 1
        && failed[0].starts_with(&format!("FAIL {item}: "))
        && failed[0].contains(text);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name}: want only FAIL {item}: ..{text}.., got:\n{}",
            r.text
        ))
    }
}

pub fn assert_only_fails(item: &str, drift: fn(&mut FakeMac), text: &str) {
    if let Err(e) = only_fails(&format!("only-{item}"), item, drift, text) {
        panic!("{e}");
    }
}

pub fn apply_keys(mac: &FakeMac, keys: Option<&str>) -> Report {
    let path = mac.profile_file(&mac.profile_text());
    kbf_mac_provision::cli::apply(&path, keys, mac).unwrap()
}

pub fn apply(mac: &FakeMac) -> Report {
    apply_keys(mac, None)
}

/// The value `pmset -g` reports for `key` on the fake.
pub fn pmset_value(mac: &FakeMac, key: &str) -> Option<String> {
    let s = mac.state.borrow();
    s.pmset
        .iter()
        .find_map(|(k, v)| (k == key).then(|| v.clone()))
}

/// The fake's `defaults` value of `domain` and `key`.
pub fn preference(mac: &FakeMac, domain: &str, key: &str) -> Option<String> {
    mac.state
        .borrow()
        .defaults
        .get(&(domain.to_owned(), key.to_owned()))
        .cloned()
}

/// `lstat` of `path` on the fake, as `ls -ldn`'s fields.
pub fn ls(mac: &FakeMac, path: &str) -> String {
    kbf_mac_provision::host::Host::lstat(mac, path).map_or_else(|| "missing".into(), |s| s.ls())
}

/// The program calls in the log that start with `prefix`.
pub fn calls(mac: &FakeMac, prefix: &str) -> Vec<String> {
    mac.log
        .borrow()
        .iter()
        .filter(|c| c.starts_with(prefix))
        .cloned()
        .collect()
}
