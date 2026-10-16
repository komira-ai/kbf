//! The profile file: `KEY=VALUE` lines, parsed and never evaluated. Blank lines and
//! lines starting with `#` are skipped; the last line must end in a newline. A key is
//! matched exactly (every key is lower-case letters and `_`). `xcode`, `probe` and
//! `expect_probe` may repeat; every other key appears at most once, every key of
//! [`REQUIRED`] exactly once, and a key outside [`REQUIRED`], [`OPTIONAL`] and the
//! repeated three is refused. A value may hold only letters, digits, spaces and
//! `._:/=@+,-`, and each key's value has a shape of its own (this crate's README.md
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
    "kbf_data_dir",
    "kbf_labels",
    "simulator_runtimes",
];

/// The keys a profile may leave out (or leave empty), shown after [`REQUIRED`].
pub const OPTIONAL: &[&str] = &[
    "default_developer_dir",
    "xcode_xip_dir",
    "pool_users",
    "pool_first_uid",
    "kbf_volume",
    "kbf_cache",
    "daemon_user",
];

/// How many lease users the pool has when the profile does not say.
pub const POOL_USERS: u32 = 4;
/// The volume the pool's homes, the scratch root and the cache are on when the profile
/// does not say.
pub const VOLUME: &str = "/Volumes/kbf";

/// The account kbf-daemon's job runs as (`daemon_user`). A root daemon must not ship
/// before the daemon runs every action as a pool user, so `role` is the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonUser {
    /// The role account (`role_user`): the property list sets `UserName` and
    /// `GroupName`; the node key and the roots are not checked.
    Role,
    /// root: no `UserName`; the node key is root's, mode 600, and the scratch and cache
    /// roots are root's, mode 755, on the volume.
    Root,
}

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

/// The old name of `kbf_data_dir`, accepted with a warning.
const OLD_DATA_DIR: &str = "kbf_scratch";

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
    /// `pool_users`: the pool is `kbf-pool-0` .. `kbf-pool-<pool_users - 1>`.
    pub pool_users: u32,
    /// `pool_first_uid`: the uid of `kbf-pool-0` and the gid of group `kbfpool`.
    pub pool_first_uid: u32,
    /// `kbf_volume`: the mount point of the volume.
    pub volume: String,
    /// `daemon_user`: the account kbf-daemon's job runs as.
    pub daemon_user: DaemonUser,
    pub time_max_offset_ms: u64,
    /// The daemon's other `--label`s, from `kbf_labels`.
    pub labels: Vec<String>,
    /// The SHA-256 of the file, in hex.
    pub sha256: String,
    /// Deprecation warnings, one line each: the profile is accepted, and `load` prints
    /// them on stderr.
    pub warnings: Vec<String>,
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
        let profile = Self::parse(&bytes)?;
        for warning in &profile.warnings {
            eprintln!("kbf-mac-provision: warning: {warning}");
        }
        Ok(profile)
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
        let mut warnings = Vec::new();
        let mut old_line = None;
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
            if key_text == OLD_DATA_DIR {
                if old_line.is_some() {
                    return Err(ProfileError::Duplicate {
                        line: line_no,
                        key: key_text,
                    });
                }
                if values.contains_key("kbf_data_dir") {
                    return Err(both_data_keys(line_no));
                }
                old_line = Some(line_no);
                warnings.push(format!(
                    "profile line {line_no}: {OLD_DATA_DIR} is deprecated, use kbf_data_dir (treated as kbf_data_dir)"
                ));
                values.insert("kbf_data_dir", value);
                continue;
            }
            let Some(&name) = REQUIRED.iter().chain(OPTIONAL).find(|&&k| k == key_text) else {
                return Err(ProfileError::UnknownKey {
                    line: line_no,
                    key: key_text,
                });
            };
            if name == "kbf_data_dir" && old_line.is_some() {
                return Err(both_data_keys(line_no));
            }
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
        let mut profile = Self::validate(
            values,
            &xcodes,
            &probes,
            expectations,
            hex::encode(Sha256::digest(bytes)),
        )?;
        profile.warnings = warnings;
        Ok(profile)
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
        let (pool_users, pool_first_uid) = pool(&values, &range)?;
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
        for key in ["kbf_ca_cert", "kbf_cert", "kbf_key", "kbf_data_dir"] {
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
        let (volume, daemon_user) = volume_and_daemon(&values)?;
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
            pool_users,
            pool_first_uid,
            volume,
            daemon_user,
            time_max_offset_ms,
            labels,
            sha256,
            warnings: Vec::new(),
        })
    }
}

/// The refusal of a profile that sets both `kbf_data_dir` and its old name.
fn both_data_keys(line: usize) -> ProfileError {
    ProfileError::Invalid(format!(
        "profile line {line}: {OLD_DATA_DIR} and kbf_data_dir are both set; {OLD_DATA_DIR} is the old name of kbf_data_dir, keep only kbf_data_dir"
    ))
}

/// The value of `key` in `values`, empty when it is not set.
fn value<'a>(values: &'a BTreeMap<&'static str, String>, key: &str) -> &'a str {
    values.get(key).map_or("", String::as_str)
}

/// `pool_users` (1 to 64, default [`POOL_USERS`]) and `pool_first_uid` (default the
/// uid after `lease_uids`, and never below 600): the pool's uids are at least 600, at
/// most 65535, and outside `lease_uids`.
fn pool(
    values: &BTreeMap<&'static str, String>,
    lease: &RangeInclusive<u64>,
) -> Result<(u32, u32), ProfileError> {
    let get = |key| value(values, key);
    let n = match get("pool_users") {
        "" => u64::from(POOL_USERS),
        v => Some(v)
            .filter(|v| pattern::number(v))
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|n| (1..=64).contains(n))
            .ok_or_else(|| invalid("pool_users", v, "a number from 1 to 64"))?,
    };
    let first = match get("pool_first_uid") {
        "" => lease.end().saturating_add(1).max(600),
        v => Some(v)
            .filter(|v| pattern::number(v))
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| invalid("pool_first_uid", v, "a number"))?,
    };
    let last = first.saturating_add(n - 1);
    if first < 600 {
        return Err(ProfileError::Invalid(format!(
            "pool_first_uid: {first} is below 600"
        )));
    }
    let uids = format!("pool uids {first}-{last}");
    if last > 65535 {
        return Err(ProfileError::Invalid(format!("{uids} go above 65535")));
    }
    if first <= *lease.end() && last >= *lease.start() {
        return Err(ProfileError::Invalid(format!(
            "{uids} overlap lease_uids {}",
            get("lease_uids")
        )));
    }
    // Both are at most 65535.
    Ok((n as u32, first as u32))
}

/// `kbf_volume` (`/Volumes/<name>`, default [`VOLUME`]), `kbf_cache` (an absolute
/// path, if set) and `daemon_user` (`role`, the default, or `root`). For a root daemon,
/// the scratch root and the cache are on the volume.
fn volume_and_daemon(
    values: &BTreeMap<&'static str, String>,
) -> Result<(String, DaemonUser), ProfileError> {
    let get = |key| value(values, key);
    let volume = match get("kbf_volume") {
        "" => VOLUME,
        v => Some(v)
            .filter(|v| pattern::absolute_path(v))
            .filter(|v| {
                v.strip_prefix("/Volumes/")
                    .is_some_and(|name| !name.is_empty() && !name.contains('/'))
            })
            .ok_or_else(|| invalid("kbf_volume", v, "/Volumes/<name>"))?,
    };
    let cache = get("kbf_cache");
    if !cache.is_empty() && !pattern::absolute_path(cache) {
        return Err(invalid("kbf_cache", cache, "an absolute path"));
    }
    let daemon_user = match get("daemon_user") {
        "" | "role" => DaemonUser::Role,
        "root" => DaemonUser::Root,
        v => return Err(invalid("daemon_user", v, "role or root")),
    };
    if daemon_user == DaemonUser::Root {
        for key in ["kbf_data_dir", "kbf_cache"] {
            let path = get(key);
            if !path.is_empty() && !path.starts_with(&format!("{volume}/")) {
                return Err(ProfileError::Invalid(format!(
                    "{key}: {path} is not on kbf_volume {volume} (daemon_user=root)"
                )));
            }
        }
    }
    Ok((volume.to_owned(), daemon_user))
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
