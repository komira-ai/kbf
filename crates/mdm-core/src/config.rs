//! The kit's settings: `KEY=VALUE` lines over [`KEYS`], checked into [`MdmConfig`]
//! before anything reads the host.
//!
//! The keys that name a site (the DNS name, the LAN, the user and directories, the
//! organisation, the profile identifier) have no default, because each default would
//! be one site's value. The keys that name the kit's own parts (the unit prefix, the
//! slice, the SCEP CA's and the DEP certificate's names, the ADE profile's text and the
//! signature namespace) have a default derived from the unit prefix and the
//! organisation, so a site that wants no particular names sets none. `config.example`
//! in this crate documents every key.
//!
//! The ACME keys are read by [`crate::acme::Acme`]; `MDM_ERASE_STATE`,
//! `MDM_ERASE_PRINCIPAL` and `MDM_ERASE_MAX_AGE` are accepted and not read here, so one
//! file serves the whole kit.

use std::net::Ipv4Addr;

use crate::path::AbsPath;
use crate::settings::{Settings, SettingsError};

/// Every key the settings file may set.
pub const KEYS: &[&str] = &[
    "MDM_DNS_NAME",
    "MDM_BIND",
    "MDM_LAN_CIDR",
    "MDM_USER",
    "MDM_STATE",
    "MDM_ETC",
    "MDM_BIN",
    "MDM_ERASE_STATE",
    "MDM_NANOHUB_PORT",
    "MDM_SCEP_PORT",
    "MDM_DEP_PORT",
    "MDM_MEMORY_MAX",
    "MDM_CPU_QUOTA",
    "MDM_TASKS_MAX",
    "MDM_UNIT_PREFIX",
    "MDM_SLICE",
    "MDM_DEP_NAME",
    "MDM_ORG",
    "MDM_PROFILE_ID",
    "MDM_PROFILE_DISPLAY_NAME",
    "MDM_ADE_PROFILE_NAME",
    "MDM_ADE_ORG_MAGIC",
    "MDM_ADE_SKIP_ITEMS",
    "MDM_LANGUAGE",
    "MDM_REGION",
    "MDM_SCEP_CA_CN",
    "MDM_SCEP_CA_OU",
    "MDM_SCEP_CA_YEARS",
    "MDM_DEP_CERT_CN",
    "MDM_DEP_CERT_DAYS",
    "MDM_ACME_EMAIL",
    "MDM_ACME_SERVER",
    "MDM_ACME_DNS_PROVIDER",
    "MDM_ACME_RESOLVERS",
    "MDM_ACME_PROPAGATION_WAIT",
    "MDM_SIGNATURE_NAMESPACE",
    "MDM_ERASE_PRINCIPAL",
    "MDM_ERASE_MAX_AGE",
];

/// The default unit prefix: the kit's own name.
pub const DEFAULT_UNIT_PREFIX: &str = "js-mdm";

/// The Setup Assistant panes an enrolled Mac skips unless `MDM_ADE_SKIP_ITEMS` says
/// otherwise.
pub const DEFAULT_SKIP_ITEMS: [&str; 18] = [
    "Accessibility",
    "Appearance",
    "AppleID",
    "AppStore",
    "Biometric",
    "Diagnostics",
    "DisplayTone",
    "FileVault",
    "iCloudDiagnostics",
    "iCloudStorage",
    "Intelligence",
    "Privacy",
    "ScreenTime",
    "Siri",
    "TermsOfAddress",
    "TOS",
    "UnlockWithWatch",
    "Wallpaper",
];

/// The settings the unit, profile and command builders read, checked. [`MdmConfig::parse`]
/// is the only entry that checks: a value built field by field skips every check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdmConfig {
    /// The name the Macs dial; the TLS certificate is for it.
    pub dns_name: String,
    /// The LAN address the HTTPS proxy binds: private, inside `lan`.
    pub bind: Ipv4Addr,
    /// The LAN, the only network besides loopback the proxy talks to.
    pub lan: Cidr,
    /// The services' system user.
    pub user: String,
    /// State: database, SCEP CA, DEP token, TLS key.
    pub state: AbsPath,
    /// Unit inputs and the env files.
    pub etc: AbsPath,
    /// The pinned binaries.
    pub bin: AbsPath,
    /// NanoHUB's loopback port.
    pub nanohub_port: u16,
    /// The SCEP server's loopback port.
    pub scep_port: u16,
    /// NanoDEP's depserver loopback port.
    pub dep_port: u16,
    /// The slice's memory cap, as systemd writes it: `1G`.
    pub memory_max: String,
    /// The slice's CPU cap: `100%`.
    pub cpu_quota: String,
    /// The slice's task cap: `256`.
    pub tasks_max: String,
    /// The prefix of every unit name.
    pub unit_prefix: String,
    /// The slice's name, ending in `.slice`.
    pub slice: String,
    /// NanoDEP's name for the Apple Business Manager token.
    pub dep_name: String,
    /// The organisation shown with the SCEP CA and the profiles.
    pub org: String,
    /// The enrollment profile's identifier, reverse-DNS (`com.example.mdm`).
    pub profile_id: String,
    /// The enrollment profile's name in System Settings.
    pub profile_display_name: String,
    /// The ADE profile: name, org_magic, language, region, skipped panes.
    pub ade: AdeSettings,
    /// The SCEP CA's subject and validity.
    pub scep_ca: ScepCa,
    /// The DEP token certificate's name and validity.
    pub dep_cert: DepCert,
    /// The ssh signature namespace of an erase request.
    pub signature_namespace: String,
}

/// The ADE profile's settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdeSettings {
    /// The profile's name at Apple.
    pub profile_name: String,
    /// The profile's `org_magic`.
    pub org_magic: String,
    /// The language, two lower-case letters.
    pub language: String,
    /// The region, two upper-case letters.
    pub region: String,
    /// The Setup Assistant panes skipped, in the order given.
    pub skip_items: Vec<String>,
}

/// The SCEP CA's subject and validity. The CA is the issuer of every Mac's MDM identity
/// certificate, so it is fixed before the first Mac enrolls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScepCa {
    /// The common name.
    pub common_name: String,
    /// The organizational unit.
    pub organizational_unit: String,
    /// The validity in years.
    pub years: u32,
}

/// The DEP token certificate's name and validity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepCert {
    /// The common name.
    pub common_name: String,
    /// The validity in days.
    pub days: u32,
}

/// A private IPv4 network, `a.b.c.d/bits`. [`MdmConfig::parse`] takes 8 to 30 bits;
/// [`Cidr::contains`] is defined for 0 to 32.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    /// The network address.
    pub addr: Ipv4Addr,
    /// The prefix length.
    pub bits: u8,
}

impl Cidr {
    /// Whether `a` is inside the network.
    pub fn contains(self, a: Ipv4Addr) -> bool {
        // `bits` 0 shifts by 32, which `checked_shl` refuses: the mask is then empty.
        let shift = 32u32.saturating_sub(u32::from(self.bits));
        let mask = u32::MAX.checked_shl(shift).unwrap_or(0);
        u32::from(a) & mask == u32::from(self.addr) & mask
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.bits)
    }
}

/// Why the settings were refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The file is not `KEY=VALUE` over the closed key set, or lacks a required key.
    #[error(transparent)]
    Settings(#[from] SettingsError),
    /// A key whose value is not acceptable.
    #[error("MDM_{key}={value}: {why}")]
    Bad {
        /// The key without its `MDM_` prefix.
        key: &'static str,
        /// The value read.
        value: String,
        /// What is wrong with it.
        why: &'static str,
    },
}

pub(crate) fn bad(key: &'static str, value: &str, why: &'static str) -> ConfigError {
    ConfigError::Bad {
        key,
        value: value.to_owned(),
        why,
    }
}

/// Refuse `value` of `key` with `why` unless `ok`.
pub(crate) fn check(
    key: &'static str,
    value: &str,
    ok: bool,
    why: &'static str,
) -> Result<(), ConfigError> {
    if ok {
        Ok(())
    } else {
        Err(bad(key, value, why))
    }
}

/// Reads keys by their name without the `MDM_` prefix.
pub(crate) struct Reader<'a>(pub(crate) &'a Settings);

impl Reader<'_> {
    /// A required key's value.
    pub(crate) fn get(&self, key: &str) -> Result<String, ConfigError> {
        Ok(self.0.require(&format!("MDM_{key}"))?.to_owned())
    }

    /// A key's value, `default` when the file does not set it.
    pub(crate) fn get_or(&self, key: &str, default: &str) -> Result<String, ConfigError> {
        Ok(self.0.get_or(&format!("MDM_{key}"), default)?)
    }

    fn path(&self, key: &'static str) -> Result<AbsPath, ConfigError> {
        let text = self.get(key)?;
        check(
            key,
            &text,
            is_word(&text, "/._-"),
            "a path the units cannot carry (letters, digits, / . _ -)",
        )?;
        AbsPath::new(&text).map_err(|_| {
            bad(
                key,
                &text,
                "not an absolute path with no empty, . or .. part",
            )
        })
    }

    fn port(&self, key: &'static str) -> Result<u16, ConfigError> {
        let text = self.get(key)?;
        let port = text.parse::<u16>().ok().filter(|p| *p != 0);
        port.ok_or(bad(key, &text, "not a port from 1 to 65535"))
    }

    /// A systemd cap: digits from 1 (no leading zero), then one of `suffixes`.
    fn capped(
        &self,
        key: &'static str,
        suffixes: &[char],
        why: &'static str,
    ) -> Result<String, ConfigError> {
        let text = self.get(key)?;
        check(key, &text, is_count(&text, suffixes), why)?;
        Ok(text)
    }

    /// A whole number in `1..=max`, `default` when the file does not set it.
    fn number(&self, key: &'static str, default: u32, max: u32) -> Result<u32, ConfigError> {
        let text = self.get_or(key, &default.to_string())?;
        let n = text.parse::<u32>().ok().filter(|n| (1..=max).contains(n));
        n.ok_or(bad(key, &text, "not a whole number in the allowed range"))
    }

    /// A name of letters, digits and `extra`, `default` when the file does not set it.
    fn name(
        &self,
        key: &'static str,
        default: &str,
        extra: &str,
        why: &'static str,
    ) -> Result<String, ConfigError> {
        let text = self.get_or(key, default)?;
        check(key, &text, is_name(&text, extra), why)?;
        Ok(text)
    }

    /// Free text without a control character, `default` when the file does not set it.
    fn text(&self, key: &'static str, default: &str) -> Result<String, ConfigError> {
        let text = self.get_or(key, default)?;
        check(
            key,
            &text,
            !text.chars().any(char::is_control),
            "holds a control character",
        )?;
        Ok(text)
    }
}

impl MdmConfig {
    /// Check the settings in `text`.
    pub fn parse(text: &str) -> Result<MdmConfig, ConfigError> {
        let settings = Settings::parse(text, KEYS)?;
        let r = Reader(&settings);
        let dns_name = r.get("DNS_NAME")?;
        check(
            "DNS_NAME",
            &dns_name,
            is_dns_name(&dns_name),
            "not a lower-case DNS name with a dot",
        )?;
        let text = r.get("BIND")?;
        let bind = lan_address(&text).ok_or(bad(
            "BIND",
            &text,
            "not a private LAN address (an RFC 1918 range): the proxy binds the LAN only, never 0.0.0.0, loopback or another range",
        ))?;
        let text = r.get("LAN_CIDR")?;
        let lan = cidr(&text).ok_or(bad(
            "LAN_CIDR",
            &text,
            "not a private IPv4 network in CIDR form inside one RFC 1918 block (at least 8 bits in 10/8, 12 in 172.16/12, 16 in 192.168/16; at most 30)",
        ))?;
        check(
            "BIND",
            &bind.to_string(),
            lan.contains(bind),
            "not inside MDM_LAN_CIDR (a container bridge's or another network's address?)",
        )?;
        let user = r.get("USER")?;
        check(
            "USER",
            &user,
            is_user_name(&user),
            "not a user name ([a-z_][a-z0-9_-]{0,30})",
        )?;
        // The units' sandboxing assumes an account without privilege.
        check(
            "USER",
            &user,
            !["root", "toor"].contains(&user.as_str()),
            "a superuser account: the services run unprivileged",
        )?;
        let (state, etc, bin) = (r.path("STATE")?, r.path("ETC")?, r.path("BIN")?);
        // The state is the service user's to write: no other directory may be inside it,
        // or equal to it, or contain it (a binary the user can replace is not pinned).
        for (key, path, other, why) in [
            ("ETC", &etc, &state, "equal to, inside or around MDM_STATE"),
            ("BIN", &bin, &state, "equal to, inside or around MDM_STATE"),
            ("BIN", &bin, &etc, "equal to, inside or around MDM_ETC"),
        ] {
            let clash = nested(path.as_str(), other.as_str());
            check(key, path.as_str(), !clash, why)?;
        }
        let (nanohub_port, scep_port, dep_port) = (
            r.port("NANOHUB_PORT")?,
            r.port("SCEP_PORT")?,
            r.port("DEP_PORT")?,
        );
        let memory_max = r.capped("MEMORY_MAX", &['K', 'M', 'G'], "not like 1G")?;
        let cpu_quota = r.capped("CPU_QUOTA", &['%'], "not like 100%")?;
        let tasks_max = r.capped("TASKS_MAX", &[], "not a number from 1")?;
        let unit_prefix = r.get_or("UNIT_PREFIX", DEFAULT_UNIT_PREFIX)?;
        check(
            "UNIT_PREFIX",
            &unit_prefix,
            is_unit_stem(&unit_prefix),
            "not lower-case letters, digits and - from a letter, at most 40 characters",
        )?;
        let slice = r.get_or("SLICE", &format!("{unit_prefix}.slice"))?;
        let stem = slice.strip_suffix(".slice").unwrap_or_default();
        check(
            "SLICE",
            &slice,
            is_unit_stem(stem),
            "not a slice name: lower-case letters, digits and - from a letter, then .slice",
        )?;
        // systemd's own slices (`system`, `user`, `machine`, `init`, and what nests in
        // them) are not the kit's to cap; the kit's slice is its prefix or below it.
        let top = stem.split('-').next().unwrap_or_default();
        check(
            "SLICE",
            &slice,
            !["system", "user", "machine", "init"].contains(&top),
            "a slice of systemd's own",
        )?;
        check(
            "SLICE",
            &slice,
            stem == unit_prefix || stem.starts_with(&format!("{unit_prefix}-")),
            "not MDM_UNIT_PREFIX or a slice below it (MDM_UNIT_PREFIX-...)",
        )?;
        let dep_name = r.get("DEP_NAME")?;
        check(
            "DEP_NAME",
            &dep_name,
            is_name(&dep_name, "._-"),
            "not a name of letters, digits, . _ - that starts with a letter or digit",
        )?;
        let org = r.get("ORG")?;
        check(
            "ORG",
            &org,
            !org.chars().any(char::is_control),
            "holds a control character",
        )?;
        let profile_id = r.get("PROFILE_ID")?;
        check(
            "PROFILE_ID",
            &profile_id,
            is_dns_name(&profile_id),
            "not a reverse-DNS identifier (lower-case labels joined by dots)",
        )?;
        let profile_display_name = r.text("PROFILE_DISPLAY_NAME", &format!("{org} MDM"))?;
        let ade = AdeSettings {
            profile_name: r.text("ADE_PROFILE_NAME", &format!("{org} MDM"))?,
            org_magic: r.name(
                "ADE_ORG_MAGIC",
                &unit_prefix,
                "._-",
                "not a name of letters, digits, . _ - that starts with a letter or digit",
            )?,
            language: r.get_or("LANGUAGE", "en")?,
            region: r.get_or("REGION", "US")?,
            skip_items: skip_items(&r.get_or("ADE_SKIP_ITEMS", &DEFAULT_SKIP_ITEMS.join(" "))?)?,
        };
        check(
            "LANGUAGE",
            &ade.language,
            is_letters(&ade.language, char::is_ascii_lowercase),
            "not two lower-case letters",
        )?;
        check(
            "REGION",
            &ade.region,
            is_letters(&ade.region, char::is_ascii_uppercase),
            "not two upper-case letters",
        )?;
        let scep_ca = ScepCa {
            common_name: r.text("SCEP_CA_CN", &format!("{org} MDM SCEP CA"))?,
            organizational_unit: r.text("SCEP_CA_OU", &unit_prefix)?,
            years: r.number("SCEP_CA_YEARS", 10, 30)?,
        };
        let dep_cert = DepCert {
            common_name: r.name(
                "DEP_CERT_CN",
                &unit_prefix,
                "._-",
                "not a name of letters, digits, . _ - that starts with a letter or digit",
            )?,
            days: r.number("DEP_CERT_DAYS", 7, 365)?,
        };
        let signature_namespace = r.name(
            "SIGNATURE_NAMESPACE",
            &format!("{unit_prefix}-erase"),
            "._-",
            "not a name of letters, digits, . _ - that starts with a letter or digit",
        )?;
        Ok(MdmConfig {
            dns_name,
            bind,
            lan,
            user,
            state,
            etc,
            bin,
            nanohub_port,
            scep_port,
            dep_port,
            memory_max,
            cpu_quota,
            tasks_max,
            unit_prefix,
            slice,
            dep_name,
            org,
            profile_id,
            profile_display_name,
            ade,
            scep_ca,
            dep_cert,
            signature_namespace,
        })
    }
}

/// The skipped panes: words of letters and digits, at least one.
fn skip_items(text: &str) -> Result<Vec<String>, ConfigError> {
    let items: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    let ok = !items.is_empty() && items.iter().all(|i| is_word(i, ""));
    check(
        "ADE_SKIP_ITEMS",
        text,
        ok,
        "not Setup Assistant pane names (letters and digits) separated by spaces",
    )?;
    Ok(items)
}

/// A private LAN address: one of the RFC 1918 ranges.
pub fn lan_address(text: &str) -> Option<Ipv4Addr> {
    let a: Ipv4Addr = text.parse().ok()?;
    a.is_private().then_some(a)
}

fn cidr(text: &str) -> Option<Cidr> {
    let (addr, bits) = text.split_once('/')?;
    let addr = lan_address(addr)?;
    // The network must stay inside the private block the address is in: a /8 of
    // the 192.168/16 block would be read by systemd as the /8 of 192, which is public space.
    let block_bits = match addr.octets()[0] {
        10 => 8,
        172 => 12,
        _ => 16,
    };
    let bits: u8 = bits
        .parse()
        .ok()
        .filter(|b| (block_bits..=30).contains(b))?;
    Some(Cidr { addr, bits })
}

/// Whether `a` and `b` are the same directory or one is inside the other.
fn nested(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// A lower-case DNS name with at least one dot; no label starts or ends with `-`.
fn is_dns_name(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// A whole number from 1, without leading zeros, then one of `suffixes` (none when
/// `suffixes` is empty).
fn is_count(text: &str, suffixes: &[char]) -> bool {
    let digits = if suffixes.is_empty() {
        Some(text)
    } else {
        text.strip_suffix(suffixes)
    };
    digits.is_some_and(|d| {
        !d.is_empty() && !d.starts_with('0') && d.chars().all(|c| c.is_ascii_digit())
    })
}

fn is_user_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 31
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Lower-case letters, digits and `-`, starting with a letter, at most 40 characters:
/// what a systemd unit name's stem is made of here.
fn is_unit_stem(name: &str) -> bool {
    let mut chars = name.chars();
    // systemd reads `-` in a slice name as a nesting step and refuses an empty one.
    name.len() <= 40
        && !name.ends_with('-')
        && !name.contains("--")
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A word that does not start with `-` (a flag to the program that takes it) or `.` (a
/// path step in the URL that carries it).
pub(crate) fn is_name(text: &str, extra: &str) -> bool {
    is_word(text, extra) && !text.starts_with(['-', '.'])
}

/// Only letters, digits and the characters of `extra` (the callers hold values that
/// are not empty: `require` refuses an empty one).
pub(crate) fn is_word(text: &str, extra: &str) -> bool {
    text.chars()
        .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Exactly two ASCII letters, all passing `case`.
fn is_letters(text: &str, case: fn(&char) -> bool) -> bool {
    text.chars().count() == 2 && text.chars().all(|c| case(&c))
}
