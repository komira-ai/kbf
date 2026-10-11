//! The profile file: `KEY=VALUE` lines, parsed and never evaluated. Blank lines and
//! lines starting with `#` are skipped; the last line must end in a newline. A key is
//! matched exactly (every key is lower-case letters and `_`). `xcode`, `probe` and
//! `expect_probe` may repeat; every other key appears at most once, every key of
//! [`REQUIRED`] exactly once, and a key outside [`REQUIRED`], [`OPTIONAL`] and the
//! repeated three is refused. A value may hold only letters, digits, spaces and
//! `._:/=@+,-`, and each key's value has a shape of its own (tools/mac-provision/README.md
//! lists them).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::pattern;

/// The keys every profile sets, in the order `print` shows them.
pub const REQUIRED: &[&str] = &[
    "profile",
    "macos_version",
    "macos_build",
    "hostname",
    "admin_user",
    "role_user",
    "role_id",
    "lease_uids",
    "power_sleep",
    "power_standby",
    "power_powernap",
    "power_autorestart",
    "power_womp",
    "updates_auto_download",
    "updates_auto_install_macos",
    "updates_auto_install_security",
    "updates_config_data",
    "appstore_auto_update",
    "remote_login",
    "ssh_password_auth",
    "firewall",
    "filevault",
    "autologin",
    "time_server",
    "time_max_offset_ms",
    "launchd_label",
    "kbf_server",
    "kbf_cas",
    "kbf_ca_cert",
    "kbf_cert",
    "kbf_key",
    "kbf_scratch",
    "kbf_labels",
    "simulator_runtimes",
];

/// The keys a profile may leave out (or leave empty), shown after [`REQUIRED`].
pub const OPTIONAL: &[&str] = &["default_developer_dir", "xcode_xip_dir"];

/// Why a profile is refused. Every refusal exits with status 2.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("no readable profile file: {0}")]
    Unreadable(String),
    #[error("the profile's last line has no newline")]
    NoFinalNewline,
    #[error("profile line {line}: not KEY=VALUE")]
    NotKeyValue { line: usize },
    #[error("profile line {line}: unknown key: {key}")]
    UnknownKey { line: usize, key: String },
    #[error(
        "profile line {line}: {key}: a value may hold only letters, digits, spaces and ._:/=@+,-"
    )]
    BadCharacter { line: usize, key: String },
    #[error("profile line {line}: duplicate key: {key}")]
    Duplicate { line: usize, key: String },
    #[error("missing key: {0}")]
    Missing(&'static str),
    /// A value of the wrong shape, or values that contradict each other.
    #[error("{0}")]
    Invalid(String),
}

/// A pinned Xcode: `BUILD:/Applications/Xcode*.app/Contents/Developer:SHA256`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Xcode {
    pub build: String,
    pub developer_dir: String,
    /// The SHA-256 of the `.xip` it installs from.
    pub sha256: String,
}

impl Xcode {
    /// The application: the developer directory without `/Contents/Developer`.
    pub fn app(&self) -> &str {
        self.developer_dir
            .strip_suffix("/Contents/Developer")
            .unwrap_or(&self.developer_dir)
    }
}

/// A probe script pinned by its SHA-256: `KEY:/PATH:SHA256`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub key: String,
    pub path: String,
    pub sha256: String,
}

/// A profile that passed every rule.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Every key of [`REQUIRED`] and [`OPTIONAL`] the file sets, with its value.
    values: BTreeMap<&'static str, String>,
    pub xcodes: Vec<Xcode>,
    pub probes: Vec<Probe>,
    /// The `expect_probe` lines, as written.
    pub expectations: Vec<String>,
    pub role_id: u32,
    pub lease_uids: RangeInclusive<u64>,
    pub time_max_offset_ms: u64,
    /// The daemon's other `--label`s, from `kbf_labels`.
    pub labels: Vec<String>,
    /// The SHA-256 of the file, in hex.
    pub sha256: String,
}

fn invalid(key: &str, value: &str, what: &str) -> ProfileError {
    ProfileError::Invalid(format!("{key}: {value} is not {what}"))
}

/// A byte a value may hold.
fn value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"._:/=@+, -".contains(&b)
}

impl Profile {
    /// Reads and parses the profile at `path`.
    ///
    /// # Errors
    /// The file cannot be read, or the profile is refused.
    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        let bytes = std::fs::read(path)
            .map_err(|_| ProfileError::Unreadable(path.display().to_string()))?;
        Self::parse(&bytes)
    }

    /// Parses a profile's bytes.
    ///
    /// # Errors
    /// The profile is refused; the error says which line or key, and why.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProfileError> {
        let body = match bytes {
            [] => bytes,
            [body @ .., b'\n'] => body,
            _ => return Err(ProfileError::NoFinalNewline),
        };
        let mut values = BTreeMap::new();
        let mut repeated: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (index, line) in body.split(|&b| b == b'\n').enumerate() {
            let line_no = index + 1;
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            let Some(eq) = line.iter().position(|&b| b == b'=') else {
                return Err(ProfileError::NotKeyValue { line: line_no });
            };
            let (key, value) = (&line[..eq], &line[eq + 1..]);
            let key_text = String::from_utf8_lossy(key).into_owned();
            if !value.iter().all(|&b| value_byte(b)) {
                return Err(ProfileError::BadCharacter {
                    line: line_no,
                    key: key_text,
                });
            }
            // Every byte is ASCII by now.
            let value = String::from_utf8_lossy(value).into_owned();
            if let Some(&name) = ["xcode", "probe", "expect_probe"]
                .iter()
                .find(|&&r| r == key_text)
            {
                repeated.entry(name).or_default().push(value);
                continue;
            }
            let Some(&name) = REQUIRED.iter().chain(OPTIONAL).find(|&&k| k == key_text) else {
                return Err(ProfileError::UnknownKey {
                    line: line_no,
                    key: key_text,
                });
            };
            if values.insert(name, value).is_some() {
                return Err(ProfileError::Duplicate {
                    line: line_no,
                    key: key_text,
                });
            }
        }
        if let Some(&missing) = REQUIRED.iter().find(|&&k| !values.contains_key(k)) {
            return Err(ProfileError::Missing(missing));
        }
        let mut take = |name| repeated.remove(name).unwrap_or_default();
        let (xcodes, probes, expectations) = (take("xcode"), take("probe"), take("expect_probe"));
        Self::validate(
            values,
            &xcodes,
            &probes,
            expectations,
            hex::encode(Sha256::digest(bytes)),
        )
    }

    /// The value of `key`, empty when the profile leaves an optional key out.
    pub fn get(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }

    /// The first 12 hex digits of the profile's SHA-256.
    pub fn short_sha(&self) -> &str {
        &self.sha256[..12]
    }

    /// The node label: `profile=<profile>@<first 12 hex digits of the SHA-256>`.
    pub fn node_label(&self) -> String {
        format!("profile={}@{}", self.get("profile"), self.short_sha())
    }

    /// What `print` shows: each key set to a non-empty value (required, then optional),
    /// the repeated keys, the SHA-256 and the node label.
    pub fn print(&self) -> String {
        let mut out = String::new();
        for key in REQUIRED.iter().chain(OPTIONAL) {
            let value = self.get(key);
            if !value.is_empty() {
                out.push_str(&format!("{key}={value}\n"));
            }
        }
        for x in &self.xcodes {
            out.push_str(&format!(
                "xcode={}:{}:{}\n",
                x.build, x.developer_dir, x.sha256
            ));
        }
        for p in &self.probes {
            out.push_str(&format!("probe={}:{}:{}\n", p.key, p.path, p.sha256));
        }
        for e in &self.expectations {
            out.push_str(&format!("expect_probe={e}\n"));
        }
        out.push_str(&format!(
            "profile_sha256={}\nnode_label={}\n",
            self.sha256,
            self.node_label()
        ));
        out
    }

    fn validate(
        values: BTreeMap<&'static str, String>,
        xcodes: &[String],
        probes: &[String],
        expectations: Vec<String>,
        sha256: String,
    ) -> Result<Self, ProfileError> {
        let get = |key: &str| values.get(key).map_or("", String::as_str);
        let want = |key: &str, ok: fn(&str) -> bool, what: &str| {
            if ok(get(key)) {
                Ok(())
            } else {
                Err(invalid(key, get(key), what))
            }
        };
        want("profile", pattern::profile_name, "a profile name")?;
        want("macos_version", pattern::version, "a macOS version")?;
        want("macos_build", pattern::build, "a macOS build")?;
        want(
            "hostname",
            pattern::node_name,
            "a name of the form <farm>-mac-<nn> (lower-case letters, digits, -)",
        )?;
        if get("hostname").len() > 63 {
            return Err(ProfileError::Invalid(
                "hostname: longer than 63 characters".into(),
            ));
        }
        want("admin_user", pattern::account, "an account name")?;
        want(
            "role_user",
            pattern::role_account,
            "a role account name (it starts with _)",
        )?;
        want("role_id", pattern::number, "a number")?;
        let role_id = match get("role_id").parse::<u32>() {
            Ok(id) if id < 200 => {
                return Err(ProfileError::Invalid(format!("role_id: {id} is below 200")));
            }
            Ok(id) if id < 500 => id,
            _ => {
                return Err(ProfileError::Invalid(format!(
                    "role_id: {} is not a hidden role id (below 500)",
                    get("role_id")
                )));
            }
        };
        let lease = get("lease_uids");
        let range = lease
            .split_once('-')
            .filter(|(a, b)| pattern::number(a) && pattern::number(b))
            .and_then(|(a, b)| Some(a.parse::<u64>().ok()?..=b.parse::<u64>().ok()?))
            .ok_or_else(|| invalid("lease_uids", lease, "a range FIRST-LAST"))?;
        if *range.start() < 501 {
            return Err(ProfileError::Invalid(format!(
                "lease_uids: {lease} starts below 501"
            )));
        }
        if range.is_empty() {
            return Err(ProfileError::Invalid(format!(
                "lease_uids: {lease} is empty"
            )));
        }
        for key in ["power_sleep", "power_standby", "time_max_offset_ms"] {
            want(key, pattern::number, "a number")?;
        }
        let time_max_offset_ms = get("time_max_offset_ms")
            .parse::<u64>()
            .map_err(|_| invalid("time_max_offset_ms", get("time_max_offset_ms"), "a number"))?;
        for key in [
            "power_powernap",
            "power_autorestart",
            "power_womp",
            "updates_auto_download",
            "updates_auto_install_macos",
            "updates_auto_install_security",
            "updates_config_data",
            "appstore_auto_update",
            "remote_login",
            "ssh_password_auth",
        ] {
            want(key, pattern::bit, "0 or 1")?;
        }
        want("firewall", |v| v == "on" || v == "off", "on or off")?;
        want("filevault", |v| v == "on" || v == "off", "on or off")?;
        want(
            "autologin",
            |v| v == "off",
            "off (kbf-mac-session alone sets a lease user, during a lease)",
        )?;
        want(
            "simulator_runtimes",
            |v| v == "none",
            "none (simulator runtimes live in VM images only)",
        )?;
        want("time_server", pattern::host_name, "a host name")?;
        want("launchd_label", pattern::launchd_label, "a launchd label")?;
        want(
            "kbf_server",
            pattern::https_url,
            "an https://host[:port] URL",
        )?;
        want("kbf_cas", pattern::https_url, "an https://host[:port] URL")?;
        for key in ["kbf_ca_cert", "kbf_cert", "kbf_key", "kbf_scratch"] {
            want(key, pattern::absolute_path, "an absolute path")?;
        }
        let labels: Vec<String> = get("kbf_labels")
            .split(' ')
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        for label in &labels {
            if !pattern::daemon_label(label) {
                return Err(ProfileError::Invalid(format!(
                    "kbf_labels: {label} is not key=value"
                )));
            }
            if label.starts_with("profile=") {
                return Err(ProfileError::Invalid(
                    "kbf_labels: the profile label is kbf-mac-provision's".into(),
                ));
            }
        }
        if !get("xcode_xip_dir").is_empty() {
            want("xcode_xip_dir", pattern::absolute_path, "an absolute path")?;
        }
        let xcodes = parse_xcodes(xcodes, get("default_developer_dir"))?;
        let probes = parse_probes(probes)?;
        check_expectations(&expectations, &probes, &xcodes)?;
        Ok(Self {
            values,
            xcodes,
            probes,
            expectations,
            role_id,
            lease_uids: range,
            time_max_offset_ms,
            labels,
            sha256,
        })
    }
}

fn parse_xcodes(lines: &[String], default_dir: &str) -> Result<Vec<Xcode>, ProfileError> {
    let mut xcodes: Vec<Xcode> = Vec::new();
    for line in lines {
        let shaped = match line.split(':').collect::<Vec<_>>()[..] {
            [build, dir, sha]
                if pattern::build(build)
                    && pattern::xcode_developer_dir(dir)
                    && pattern::sha256(sha) =>
            {
                Some(Xcode {
                    build: build.into(),
                    developer_dir: dir.into(),
                    sha256: sha.into(),
                })
            }
            _ => None,
        };
        let x = shaped.ok_or_else(|| {
            ProfileError::Invalid(format!(
                "xcode: {line} is not BUILD:/Applications/Xcode*.app/Contents/Developer:SHA256"
            ))
        })?;
        if xcodes
            .iter()
            .any(|o| o.build == x.build || o.developer_dir == x.developer_dir)
        {
            return Err(ProfileError::Invalid(format!(
                "xcode: {} or {} is pinned twice",
                x.build, x.developer_dir
            )));
        }
        xcodes.push(x);
    }
    if xcodes.is_empty() {
        if !default_dir.is_empty() {
            return Err(ProfileError::Invalid(
                "default_developer_dir: no xcode is pinned".into(),
            ));
        }
    } else if !xcodes.iter().any(|x| x.developer_dir == default_dir) {
        return Err(ProfileError::Invalid(format!(
            "default_developer_dir: {default_dir} is not the path of a pinned xcode"
        )));
    }
    Ok(xcodes)
}

fn parse_probes(lines: &[String]) -> Result<Vec<Probe>, ProfileError> {
    let mut probes: Vec<Probe> = Vec::new();
    for line in lines {
        let p = match line.split(':').collect::<Vec<_>>()[..] {
            [key, path, sha]
                if pattern::probe_key(key)
                    && pattern::absolute_path(path)
                    && pattern::sha256(sha) =>
            {
                Probe {
                    key: key.into(),
                    path: path.into(),
                    sha256: sha.into(),
                }
            }
            _ => {
                return Err(ProfileError::Invalid(format!(
                    "probe: {line} is not KEY:/PATH:SHA256"
                )));
            }
        };
        if probes.iter().any(|o| o.key == p.key) {
            return Err(ProfileError::Invalid(format!(
                "probe: {} is named twice",
                p.key
            )));
        }
        probes.push(p);
    }
    Ok(probes)
}

/// Each `expect_probe` line is `KEY:XCODE_BUILD=VALUE`, names a probe and a pinned
/// Xcode, and names that pair once.
fn check_expectations(
    lines: &[String],
    probes: &[Probe],
    xcodes: &[Xcode],
) -> Result<(), ProfileError> {
    let mut seen = BTreeSet::new();
    for line in lines {
        let Some((key, build)) = line
            .split_once(':')
            .and_then(|(key, rest)| Some((key, rest.split_once('=')?)))
            .filter(|(key, (build, value))| {
                pattern::probe_key(key) && pattern::build(build) && pattern::probe_value(value)
            })
            .map(|(key, (build, _))| (key, build))
        else {
            return Err(ProfileError::Invalid(format!(
                "expect_probe: {line} is not KEY:XCODE_BUILD=VALUE"
            )));
        };
        if !probes.iter().any(|p| p.key == key) {
            return Err(ProfileError::Invalid(format!(
                "expect_probe: {key} names no probe"
            )));
        }
        if !xcodes.iter().any(|x| x.build == build) {
            return Err(ProfileError::Invalid(format!(
                "expect_probe: {build} names no pinned xcode"
            )));
        }
        if !seen.insert((key, build)) {
            return Err(ProfileError::Invalid(format!(
                "expect_probe: {key}:{build} is named twice"
            )));
        }
    }
    Ok(())
}
