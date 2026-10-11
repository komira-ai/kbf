//! Lint for `devicectl` fixtures: no file under [`FIXTURE_DIR`] may carry a
//! real-looking device identifier (docs/design/ios-devices.md, section 11: fixtures
//! are scrubbed of UDID, ECID, serial number, CoreDevice id and device name).
//!
//! Every line of a fixture is read as text, JSON or not, so a value nested at any
//! depth of a JSON file is still on some line. The text rules refuse:
//! - a UDID, `XXXXXXXX-XXXXXXXXXXXXXXXX` (8 and 16 hex digits) or the older 40 hex
//!   digits, in upper, lower or mixed case;
//! - a CoreDevice id, a UUID `8-4-4-4-12` hex digits, in any case;
//! - an ECID: the word `ecid` (any case), then `:` or `=` (quotes and spaces around
//!   it allowed), then a decimal or `0x` hex number;
//! - a serial number: the word `serial`, optionally followed by `number` (any case,
//!   joined by nothing, `_`, `-` or one space), then `:` or `=`, then a run of at
//!   least 8 letters and digits;
//! - a device name of the form `<word>'s iPhone` (also `iPad`, `iPod`, `Watch`,
//!   `Apple Watch`, `Apple TV`, `Mac`, `Vision Pro`), with either apostrophe.
//!
//! A hex identifier whose digits are all `0`, an ECID of `0` and a serial of all
//! `X` are the scrubbed placeholders and pass, as does the device name
//! [`NAME_PLACEHOLDER`].
//!
//! A file whose name ends in `.json` is also parsed and walked to every depth, since
//! a device name need not look like one: the value of a `deviceName` key, or of a
//! `name` key in a `deviceProperties` object, must be [`NAME_PLACEHOLDER`]; a `udid`,
//! `ecid` or `serialNumber` (also `serial_number`, `serial`) key, compared ignoring
//! case, must hold its placeholder, `null` or `""`. A `.json` fixture that does not parse is
//! refused, so a broken file cannot hide a value from the walk.
//!
//! Not covered: an identifier glued to other letters or digits (`x00008110-...`), a
//! device name in another form outside those two JSON keys, and a serial or ECID with
//! no label next to it. Those need a person reading the fixture before it is
//! committed.

use std::fmt;

use serde_json::Value;

/// The directory, relative to the repository root, whose files the lint reads.
pub const FIXTURE_DIR: &str = "crates/kbf-driver-native/tests/fixtures/devicectl/";

/// The one device name a fixture may carry.
pub const NAME_PLACEHOLDER: &str = "Device Name";

/// The device words a possessive device name ends in. `Apple Watch` and `Apple TV`
/// come before the shorter words so the longer match is reported.
const DEVICE_WORDS: &[&str] = &[
    "iPhone",
    "iPad",
    "iPod",
    "Apple Watch",
    "Apple TV",
    "Watch",
    "Vision Pro",
    "Mac",
];

/// True for a path the lint reads: anything under [`FIXTURE_DIR`].
pub fn is_fixture(path: &str) -> bool {
    path.starts_with(FIXTURE_DIR)
}

/// What a finding is; each holds the matched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Udid(String),
    CoreDeviceId(String),
    Ecid(String),
    Serial(String),
    DeviceName(String),
    /// A `.json` fixture that does not parse; holds the parser's message.
    Unparsed(String),
}

/// Where a finding is: a 1-based line, or a JSON path such as
/// `result.devices[0].deviceProperties.name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Line(usize),
    Json(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub place: Place,
    pub kind: Kind,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.place {
            Place::Line(n) => write!(f, "line {n}: ")?,
            Place::Json(p) => write!(f, "at {p}: ")?,
        }
        match &self.kind {
            Kind::Udid(s) => write!(f, "UDID {s}"),
            Kind::CoreDeviceId(s) => write!(f, "CoreDevice id {s}"),
            Kind::Ecid(s) => write!(f, "ECID {s}"),
            Kind::Serial(s) => write!(f, "serial number {s}"),
            Kind::DeviceName(s) => write!(f, "device name {s:?} (use {NAME_PLACEHOLDER:?})"),
            Kind::Unparsed(e) => write!(f, "not valid JSON ({e}), so it cannot be checked"),
        }
    }
}

/// Every finding in the fixture `path` (used for its extension) holding `text`: the
/// text rules in line order, then the JSON walk's.
pub fn scan(path: &str, text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for kind in scan_line(line) {
            out.push(Finding {
                place: Place::Line(i + 1),
                kind,
            });
        }
    }
    if path.to_ascii_lowercase().ends_with(".json") {
        match serde_json::from_str::<Value>(text) {
            Ok(doc) => walk(&doc, None, "", &mut out),
            Err(e) => out.push(Finding {
                place: Place::Line(e.line()),
                kind: Kind::Unparsed(e.to_string()),
            }),
        }
    }
    out
}

fn scan_line(line: &str) -> Vec<Kind> {
    let mut out = hex_ids(line);
    out.extend(labelled(line, "ecid", &[], is_ecid_token, Kind::Ecid));
    out.extend(labelled(
        line,
        "serial",
        &["number", "_number", "-number", " number"],
        is_serial_token,
        Kind::Serial,
    ));
    out.extend(possessive_names(line));
    out
}

/// A hex digit in either case.
fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// True when every digit of `s` (dashes and a leading `0x` aside) is `0`.
fn all_zero(s: &str) -> bool {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let mut digits = s.chars().filter(|&c| c != '-').peekable();
    digits.peek().is_some() && digits.all(|c| c == '0')
}

/// A hex identifier's shape: the lengths of its dash-separated groups, and its kind.
type IdShape = (&'static [usize], fn(String) -> Kind);

/// The shapes [`hex_ids`] matches, the CoreDevice id first since it is the longest.
const ID_SHAPES: [IdShape; 3] = [
    (&[8, 4, 4, 4, 12], Kind::CoreDeviceId),
    (&[8, 16], Kind::Udid),
    (&[40], Kind::Udid),
];

/// UDIDs and UUIDs in `line`: within each run of letters, digits and dashes, every
/// window of dash-separated groups shaped 8-16, 8-4-4-4-12 or 40, all hex.
fn hex_ids(line: &str) -> Vec<Kind> {
    let mut out = Vec::new();
    let runs = line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'));
    for run in runs.filter(|r| !r.is_empty()) {
        let groups: Vec<&str> = run.split('-').collect();
        let mut i = 0;
        while i < groups.len() {
            let found = ID_SHAPES.iter().find(|(lens, _)| {
                groups.len() - i >= lens.len()
                    && lens
                        .iter()
                        .zip(&groups[i..])
                        .all(|(&n, g)| g.len() == n && g.chars().all(is_hex))
            });
            match found {
                Some((lens, kind)) => {
                    let id = groups[i..i + lens.len()].join("-");
                    if !all_zero(&id) {
                        out.push(kind(id));
                    }
                    i += lens.len();
                }
                None => i += 1,
            }
        }
    }
    out
}

/// Values after the word `label` (any case, not inside a longer word), optionally
/// followed by one of `suffixes` (any case), then quotes or spaces holding `:` or `=`,
/// then a token of letters and digits that `is_value` accepts.
fn labelled(
    line: &str,
    label: &str,
    suffixes: &[&str],
    is_value: fn(&str) -> bool,
    kind: fn(String) -> Kind,
) -> Vec<Kind> {
    let lower = line.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut out = Vec::new();
    for (at, _) in lower.match_indices(label) {
        if at > 0 && b[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut rest = &lower[at + label.len()..];
        if let Some(s) = suffixes.iter().find(|s| rest.starts_with(*s)) {
            rest = &rest[s.len()..];
        }
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            continue;
        }
        let sep = rest
            .find(|c: char| !matches!(c, ' ' | '\t' | '"' | '\'' | ':' | '='))
            .unwrap_or(rest.len());
        if !rest[..sep].contains([':', '=']) {
            continue;
        }
        let start = line.len() - rest.len() + sep;
        let token: String = line[start..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if is_value(&token) {
            out.push(kind(token));
        }
    }
    out
}

/// A decimal or `0x` hex number other than the placeholder `0`.
fn is_ecid_token(t: &str) -> bool {
    let number = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => !h.is_empty() && h.chars().all(is_hex),
        None => !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()),
    };
    number && !all_zero(&t.to_ascii_lowercase())
}

/// Eight or more letters and digits, other than the placeholder (all `X`).
fn is_serial_token(t: &str) -> bool {
    t.len() >= 8 && !t.chars().all(|c| c == 'X')
}

/// `<word>'s <device word>` with a straight or curly apostrophe.
fn possessive_names(line: &str) -> Vec<Kind> {
    let mut out = Vec::new();
    for apostrophe in ["'s ", "\u{2019}s "] {
        for (at, _) in line.match_indices(apostrophe) {
            let owner_len: usize = line[..at]
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric())
                .map(char::len_utf8)
                .sum();
            if owner_len == 0 {
                continue;
            }
            let owner_start = at - owner_len;
            let after = &line[at + apostrophe.len()..];
            if let Some(w) = DEVICE_WORDS.iter().find(|w| after.starts_with(*w)) {
                let end = at + apostrophe.len() + w.len();
                out.push(Kind::DeviceName(line[owner_start..end].to_owned()));
            }
        }
    }
    out
}

/// Checks every key of every object in `v`, at any depth. `parent` is the key that
/// holds `v`, if any; `path` is `v`'s JSON path.
fn walk(v: &Value, parent: Option<&str>, path: &str, out: &mut Vec<Finding>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let here = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if let Some(kind) = check_key(parent, k, child) {
                    out.push(Finding {
                        place: Place::Json(here.clone()),
                        kind,
                    });
                }
                walk(child, Some(k), &here, out);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(item, parent, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// The finding for key `k` holding `v` in an object held by key `parent`, if the key
/// is one the walk checks and `v` is neither its placeholder, `null` nor `""`.
fn check_key(parent: Option<&str>, k: &str, v: &Value) -> Option<Kind> {
    let text = match v {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text.is_empty() {
        return None;
    }
    let key = k.to_ascii_lowercase();
    let in_props = parent.is_some_and(|p| p.eq_ignore_ascii_case("deviceProperties"));
    let (placeholder, kind): (bool, fn(String) -> Kind) = match key.as_str() {
        "udid" => (all_zero(&text), Kind::Udid),
        "ecid" => (all_zero(&text.to_ascii_lowercase()), Kind::Ecid),
        "serialnumber" | "serial_number" | "serial" => {
            (text.chars().all(|c| c == 'X'), Kind::Serial)
        }
        "devicename" => (text == NAME_PLACEHOLDER, Kind::DeviceName),
        "name" if in_props => (text == NAME_PLACEHOLDER, Kind::DeviceName),
        _ => return None,
    };
    (!placeholder).then_some(kind(text))
}

#[cfg(test)]
mod tests;
