//! Matching an action's capability request against a node's capabilities.
//!
//! Each request key has one typed comparison:
//!
//! | Key | Comparison |
//! |---|---|
//! | `arch` | exact |
//! | `isa_level` | at least, within the family (`x86-64-v3` is served by v3 and v4) |
//! | `cpu.feature` (repeatable) | subset: every requested feature is present |
//! | `cpus`, `mem_gib`, `nvme_gib`, `gpu` | countable: the node has at least the amount |
//! | `os`, `os_image`, `cpu.model`, `page_size`, `gpu.kind` | exact |
//! | `xcode` | membership: the node reports a set (one entry per installed Xcode build) and the request names one |
//! | `vm.image` | membership on the digest: the request names `<name>@sha256:<64 hex>`, the node reports one entry in that form per image it holds, and only the `sha256:<hex>` parts are compared |
//! | `ios.device` (`1`) with `ios.device.<attribute>` | one device: the node reports one `ios.device` entry per ready device, and **one** of them must satisfy every attribute (exact) |
//!
//! `gpu` is a count of whole GPUs (`gpu=1`). Matching compares it with the node's
//! count; the scheduler also books it, so a GPU serves one lease at a time.
//!
//! `vm.image` names a VM image by the digest of the recipe it is built from, so two
//! names for one recipe are one image, and one name for two recipes is two. A value
//! without `@sha256:<64 lowercase hex digits>` is refused, in a request and in a report.
//!
//! The report-only keys ([`REPORT_ONLY_KEYS`]: `vm.slots`, `vm.max_cpus`,
//! `vm.max_mem_gib`) are whole numbers a node reports about what it can run; a request
//! that names one is refused ([`RequestError::ReportOnly`]): a request never asks for VM
//! slots by count. The scheduler books a node's slots through the VM lease
//! kind (`kbf-lease=vm`); see `kbf-sched`.
//!
//! The reserved keys ([`RESERVED_KEYS`]: `kbf-lease`, `kbf-cpu`, `kbf-mac-admin`,
//! `kbf-book-cpus`, `kbf-book-mem-gib`) ask for a kind or a size of capacity, not a
//! capability; [`Request::parse`] skips them for the scheduler.
//!
//! `ios.device=1` asks for one iOS device (`docs/design/ios-devices.md`, section 5.3).
//! Its attributes are [`DEVICE_ATTRIBUTES`] (`ios.device.class=iPhone`,
//! `ios.device.os_version=26.0`, ...), each compared exactly, and a node matches only
//! if one device it reports satisfies all of them: two attributes met by two different
//! devices are not a match. A node that reports no device matches no device request, so
//! a daemon that predates devices (and would drop `Start.device_id`) is never given
//! one. [`Request::devices`] names the devices that satisfy a request; the scheduler
//! books one of them. Refused ([`RequestError`]): `ios.device` with any value but `1`,
//! an attribute without `ios.device=1`, an `ios.device.<attribute>` kbf does not know,
//! and `ios.device` with an `os` other than `macos`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::cpu::{Arch, CpuCaps};
use crate::level::IsaLevel;

/// A countable resource a request asks a minimum of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Consumable {
    Cpus,
    MemGib,
    NvmeGib,
    /// Whole GPUs.
    Gpus,
}

impl Consumable {
    const ALL: [Self; 4] = [Self::Cpus, Self::MemGib, Self::NvmeGib, Self::Gpus];

    /// The consumable whose key is `key`, if any.
    pub(crate) fn from_name(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == key)
    }

    /// The request key.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpus => "cpus",
            Self::MemGib => "mem_gib",
            Self::NvmeGib => "nvme_gib",
            Self::Gpus => "gpu",
        }
    }
}

impl fmt::Display for Consumable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Keys compared as exact strings.
const EXACT_KEYS: [&str; 5] = ["os", "os_image", "cpu.model", "page_size", "gpu.kind"];

/// Keys a node reports as a set and a request names one member of.
const MEMBER_KEYS: [&str; 2] = ["xcode", VM_IMAGE_KEY];

/// The VM image key: membership on the `sha256:<hex>` part of the value.
const VM_IMAGE_KEY: &str = "vm.image";

/// Keys a node reports and a request may not name: whole numbers about the VMs a node
/// can run.
pub const REPORT_ONLY_KEYS: [&str; 3] = ["vm.slots", "vm.max_cpus", "vm.max_mem_gib"];

/// Reserved keys: they ask for a kind or a size of capacity, not a capability, and are
/// read by the front, not matched.
pub const RESERVED_KEYS: [&str; 5] = [
    "kbf-lease",
    "kbf-cpu",
    "kbf-mac-admin",
    "kbf-book-cpus",
    "kbf-book-mem-gib",
];

/// The iOS device key: a request entry `ios.device=1`, and a report entry per ready
/// device. Every `ios.device.<attribute>` key begins with it and a `.`.
pub(crate) const IOS_DEVICE_KEY: &str = "ios.device";

/// The device attributes a request may name, as `ios.device.<attribute>`, each
/// compared exactly with the attribute of that name in a device's report entry.
pub const DEVICE_ATTRIBUTES: [&str; 4] = ["class", "product_type", "os_version", "os_build"];

/// The request key of each of [`DEVICE_ATTRIBUTES`], in the same order.
const DEVICE_KEYS: [&str; 4] = [
    "ios.device.class",
    "ios.device.product_type",
    "ios.device.os_version",
    "ios.device.os_build",
];

/// Whether `key` is `ios.device` or `ios.device.<anything>`. `ios.devices` and
/// `ios.simulator` are not device keys.
pub(crate) fn is_ios_device_key(key: &str) -> bool {
    key.strip_prefix(IOS_DEVICE_KEY)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// Whether `key` is compared as an exact string: one of the exact keys.
pub(crate) fn is_exact_key(key: &str) -> bool {
    EXACT_KEYS.contains(&key)
}

/// Whether `key` names a set the node reports, matched by membership.
pub(crate) fn is_member_key(key: &str) -> bool {
    MEMBER_KEYS.contains(&key)
}

/// What membership compares for a value of member key `key`: the `sha256:<hex>` part of
/// a `vm.image` value (`None` if it has none), and any other key's whole value.
pub(crate) fn member_token<'v>(key: &str, value: &'v str) -> Option<&'v str> {
    if key == VM_IMAGE_KEY {
        image_digest(value)
    } else {
        Some(value)
    }
}

/// The `sha256:<hex>` part of `<name>@sha256:<hex>`: a non-empty name with no `@` or
/// whitespace, and exactly 64 lowercase hex digits.
fn image_digest(value: &str) -> Option<&str> {
    let (name, digest) = value.split_once('@')?;
    let hex = digest.strip_prefix("sha256:")?;
    let name_ok = !name.is_empty() && !name.contains(char::is_whitespace);
    let hex_ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (name_ok && hex_ok).then_some(digest)
}

/// Whether `key` is one of kbf's own platform keys: a capability key, an iOS device
/// key, a reserved key [`Request::parse`] skips, or a report-only key it refuses. Every
/// `ios.device.<anything>` key is one, so an attribute kbf does not know reaches
/// [`Request::parse`] and is refused there rather than ignored.
pub(crate) fn is_own_key(key: &str) -> bool {
    is_capability_key(key)
        || RESERVED_KEYS.contains(&key)
        || REPORT_ONLY_KEYS.contains(&key)
        || is_ios_device_key(key)
}

/// Whether [`Request::parse`] reads `key` as a capability (reserved keys are not).
fn is_capability_key(key: &str) -> bool {
    matches!(key, "arch" | "isa_level" | "cpu.feature")
        || Consumable::from_name(key).is_some()
        || is_exact_key(key)
        || is_member_key(key)
}

/// What a node offers, as the scheduler sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCaps {
    /// Detected CPU capabilities.
    pub cpu: CpuCaps,
    /// Values compared exactly, by request key (`os`, `cpu.model`, `os_image`, ...).
    pub exact: BTreeMap<String, String>,
    /// Sets a request names one member of, by request key (`xcode`: every installed
    /// build; `vm.image`: the `sha256:<hex>` digest of every image).
    pub members: BTreeMap<String, BTreeSet<String>>,
    /// Countable capacity. A missing entry counts as zero.
    pub consumables: BTreeMap<Consumable, u64>,
    /// The drivers the node's daemon runs (`container`, `native`, ...), from its
    /// report's `drivers` entries. No request key matches them; the scheduler reads
    /// them to place a lease kind only where a driver serves it.
    pub drivers: BTreeSet<String>,
    /// The ready iOS devices, from its report's `ios.device` entries: by device id
    /// (the UDID), each device's other attributes (`class`, `os_version`, ...).
    pub devices: BTreeMap<String, BTreeMap<String, String>>,
}

impl NodeCaps {
    /// A node with only CPU capabilities: no exact values, no sets, no capacity, no
    /// driver, no device.
    #[must_use]
    pub fn new(cpu: CpuCaps) -> Self {
        Self {
            cpu,
            exact: BTreeMap::new(),
            members: BTreeMap::new(),
            consumables: BTreeMap::new(),
            drivers: BTreeSet::new(),
            devices: BTreeMap::new(),
        }
    }

    /// This node, running `drivers` as well.
    #[must_use]
    pub fn with_drivers<'d>(mut self, drivers: impl IntoIterator<Item = &'d str>) -> Self {
        self.drivers.extend(drivers.into_iter().map(str::to_owned));
        self
    }
}

/// Why a set of request properties is not a valid [`Request`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// The key is not a capability key.
    #[error("unknown capability key {0:?}")]
    UnknownKey(String),
    /// The value is not valid for the key.
    #[error("invalid value {value:?} for capability key {key:?}")]
    BadValue { key: String, value: String },
    /// A key that takes one value appears more than once.
    #[error("capability key {0:?} appears more than once")]
    Repeated(String),
    /// A `vm.image` value without a digest.
    #[error(
        "vm.image {0:?} names no digest; name an image as <name>@sha256:<64 lowercase hex digits>"
    )]
    NoImageDigest(String),
    /// The key is one a node reports and a request may not name.
    #[error("{0:?} is reported by a node and cannot be requested")]
    ReportOnly(String),
    /// An `ios.device.<attribute>` key whose attribute is not one of
    /// [`DEVICE_ATTRIBUTES`].
    #[error(
        "{0:?} is not an iOS device attribute; a request may name ios.device.class, \
         ios.device.product_type, ios.device.os_version and ios.device.os_build"
    )]
    UnknownDeviceAttribute(String),
    /// An `ios.device.<attribute>` key in a request that does not ask for a device
    /// (`ios.device=1`).
    #[error("{0:?} names an iOS device attribute, but the request has no ios.device=1")]
    DeviceAttributeAlone(String),
    /// `ios.device` with an `os` other than `macos`: devices are attached to Macs.
    #[error("ios.device asks for a device attached to a Mac, but the request asks for os {0:?}")]
    DeviceNotOnMacos(String),
}

/// What an action requires of a node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    arch: Option<Arch>,
    isa_level: Option<IsaLevel>,
    features: BTreeSet<String>,
    pub(crate) exact: BTreeMap<String, String>,
    /// By key: the value as requested, and what membership compares.
    members: BTreeMap<String, (String, String)>,
    minimums: BTreeMap<Consumable, u64>,
    /// With `ios.device=1`: the attributes one device must have, by attribute name.
    device: Option<BTreeMap<String, String>>,
}

/// One requirement a node does not meet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unmet<'a> {
    Arch {
        want: Arch,
    },
    IsaLevel {
        want: IsaLevel,
    },
    Feature(&'a str),
    Exact {
        key: &'a str,
        want: &'a str,
    },
    /// The node's set under `key` does not hold `want` (for `vm.image`, its digest).
    Member {
        key: &'a str,
        want: &'a str,
    },
    Consumable {
        what: Consumable,
        want: u64,
        have: u64,
    },
    /// The request asks for a device (`ios.device=1`) and the node reports none.
    NoDevice,
    /// No device the node reports has every attribute the request names. One entry per
    /// attribute the closest device lacks: the device with fewest unmet attributes,
    /// the first by id among equals.
    Device {
        /// The attribute (`os_version`).
        attribute: &'a str,
        /// The value asked for.
        want: &'a str,
        /// The closest device's id.
        device: String,
        /// Its value, `None` if it reports no such attribute.
        have: Option<String>,
    },
}

impl fmt::Display for Unmet<'_> {
    /// The requirement in request syntax: `arch=arm64`, `isa_level>=x86-64-v3`,
    /// `cpu.feature=avx2`, `os=macos`, `xcode=16C5032a`, `gpu>=2 (has 1)`,
    /// `ios.device=1`, `ios.device.os_version=26.0 (device <id> has 18.6)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arch { want } => write!(f, "arch={want}"),
            Self::IsaLevel { want } => write!(f, "isa_level>={want}"),
            Self::Feature(feature) => write!(f, "cpu.feature={feature}"),
            Self::Exact { key, want } | Self::Member { key, want } => write!(f, "{key}={want}"),
            Self::Consumable { what, want, have } => write!(f, "{what}>={want} (has {have})"),
            Self::NoDevice => write!(f, "{IOS_DEVICE_KEY}=1"),
            Self::Device {
                attribute,
                want,
                device,
                have,
            } => {
                write!(f, "{IOS_DEVICE_KEY}.{attribute}={want} (device {device} ")?;
                match have {
                    Some(have) => write!(f, "has {have})"),
                    None => write!(f, "has none)"),
                }
            }
        }
    }
}

impl Request {
    /// Builds a request from `(key, value)` properties; see the module table.
    ///
    /// `cpu.feature` may repeat; every other key may appear once. A membership key
    /// (`xcode`, `vm.image`) names one non-empty value; a `vm.image` value carries a
    /// digest. A report-only key is refused. `ios.device` takes `1`; an
    /// `ios.device.<attribute>` names a known attribute and a non-empty value, and only
    /// beside `ios.device=1`, which goes with no `os` but `macos`.
    pub fn parse<'p, I>(properties: I) -> Result<Self, RequestError>
    where
        I: IntoIterator<Item = (&'p str, &'p str)>,
    {
        Self::parse_each(&mut properties.into_iter())
    }

    /// [`Self::parse`], compiled once rather than once per caller's iterator type.
    fn parse_each<'p>(
        properties: &mut dyn Iterator<Item = (&'p str, &'p str)>,
    ) -> Result<Self, RequestError> {
        let mut req = Self::default();
        let mut wants_device = false;
        let mut attributes = BTreeMap::new();
        for (key, value) in properties {
            let bad = || RequestError::BadValue {
                key: key.to_owned(),
                value: value.to_owned(),
            };
            let repeated = || RequestError::Repeated(key.to_owned());
            if RESERVED_KEYS.contains(&key) {
                continue;
            }
            if REPORT_ONLY_KEYS.contains(&key) {
                return Err(RequestError::ReportOnly(key.to_owned()));
            }
            if key == IOS_DEVICE_KEY {
                if value != "1" {
                    return Err(bad());
                }
                if std::mem::replace(&mut wants_device, true) {
                    return Err(repeated());
                }
            } else if let Some(attribute) = device_attribute(key) {
                let attribute = attribute
                    .ok_or_else(|| RequestError::UnknownDeviceAttribute(key.to_owned()))?;
                if value.is_empty() {
                    return Err(bad());
                }
                if attributes.insert(attribute, value.to_owned()).is_some() {
                    return Err(repeated());
                }
            } else if key == "arch" {
                let arch = value.parse().map_err(|_| bad())?;
                set_once(&mut req.arch, arch).ok_or_else(repeated)?;
            } else if key == "isa_level" {
                let level = value.parse().map_err(|_| bad())?;
                set_once(&mut req.isa_level, level).ok_or_else(repeated)?;
            } else if key == "cpu.feature" {
                if value.is_empty() || value.contains(char::is_whitespace) {
                    return Err(bad());
                }
                req.features.insert(value.to_owned());
            } else if let Some(what) = Consumable::from_name(key) {
                let amount = value.parse().map_err(|_| bad())?;
                if req.minimums.insert(what, amount).is_some() {
                    return Err(repeated());
                }
            } else if is_exact_key(key) {
                if req.exact.insert(key.to_owned(), value.to_owned()).is_some() {
                    return Err(repeated());
                }
            } else if is_member_key(key) {
                if value.is_empty() {
                    return Err(bad());
                }
                let token = member_token(key, value)
                    .ok_or_else(|| RequestError::NoImageDigest(value.to_owned()))?;
                let member = (value.to_owned(), token.to_owned());
                if req.members.insert(key.to_owned(), member).is_some() {
                    return Err(repeated());
                }
            } else {
                return Err(RequestError::UnknownKey(key.to_owned()));
            }
        }
        if !wants_device {
            if let Some(attribute) = attributes.keys().next() {
                return Err(RequestError::DeviceAttributeAlone(format!(
                    "{IOS_DEVICE_KEY}.{attribute}"
                )));
            }
            return Ok(req);
        }
        if let Some(os) = req.exact.get("os").filter(|os| *os != "macos") {
            return Err(RequestError::DeviceNotOnMacos(os.clone()));
        }
        req.device = Some(attributes);
        Ok(req)
    }

    /// The value the request asks `key` to have exactly (`os`, `cpu.model`, ...), if it
    /// names one.
    #[must_use]
    pub fn exact(&self, key: &str) -> Option<&str> {
        self.exact.get(key).map(String::as_str)
    }

    /// The properties that [`Self::parse`] reads as this request, one per requirement,
    /// in a fixed order: `arch`, `isa_level`, each `cpu.feature`, the countable keys,
    /// the exact keys, the membership keys (each group by key, a membership key with the
    /// value as requested), then `ios.device=1` and each `ios.device.<attribute>` in
    /// [`DEVICE_ATTRIBUTES`] order. `Request::parse(r.properties())` is `r`, so a
    /// request can be written as these pairs and read back.
    #[must_use]
    pub fn properties(&self) -> Vec<(&str, String)> {
        let mut out = Vec::new();
        out.extend(self.arch.map(|a| ("arch", a.name().to_owned())));
        out.extend(self.isa_level.map(|l| ("isa_level", l.name().to_owned())));
        out.extend(self.features.iter().map(|f| ("cpu.feature", f.clone())));
        out.extend(self.minimums.iter().map(|(c, n)| (c.name(), n.to_string())));
        out.extend(self.exact.iter().map(|(k, v)| (k.as_str(), v.clone())));
        out.extend(
            self.members
                .iter()
                .map(|(k, (v, _))| (k.as_str(), v.clone())),
        );
        if let Some(attributes) = &self.device {
            out.push((IOS_DEVICE_KEY, "1".to_owned()));
            out.extend(
                DEVICE_KEYS
                    .into_iter()
                    .zip(DEVICE_ATTRIBUTES)
                    .filter_map(|(key, attribute)| Some((key, attributes.get(attribute)?.clone()))),
            );
        }
        out
    }

    /// Whether the request asks for an iOS device (`ios.device=1`).
    #[must_use]
    pub fn wants_device(&self) -> bool {
        self.device.is_some()
    }

    /// The ids of the devices `node` reports that satisfy every device attribute the
    /// request names, in id order: one of them is to be booked. Empty for a request
    /// that asks for no device.
    #[must_use]
    pub fn devices<'n>(&self, node: &'n NodeCaps) -> Vec<&'n str> {
        let Some(want) = &self.device else {
            return Vec::new();
        };
        node.devices
            .iter()
            .filter(|(_, have)| device_unmet(want, have).next().is_none())
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Every requirement the node does not meet, in a stable order. Empty means the
    /// node matches.
    #[must_use]
    pub fn unmet(&self, node: &NodeCaps) -> Vec<Unmet<'_>> {
        let mut unmet = Vec::new();
        if let Some(want) = self.arch
            && node.cpu.arch() != want
        {
            unmet.push(Unmet::Arch { want });
        }
        if let Some(want) = self.isa_level
            && !node.cpu.level().is_some_and(|have| have.satisfies(want))
        {
            unmet.push(Unmet::IsaLevel { want });
        }
        unmet.extend(
            self.features
                .iter()
                .filter(|f| !node.cpu.has(f))
                .map(|f| Unmet::Feature(f)),
        );
        for (key, want) in &self.exact {
            if node.exact.get(key) != Some(want) {
                unmet.push(Unmet::Exact { key, want });
            }
        }
        for (key, (want, token)) in &self.members {
            if !node.members.get(key).is_some_and(|set| set.contains(token)) {
                unmet.push(Unmet::Member { key, want });
            }
        }
        for (&what, &want) in &self.minimums {
            let have = node.consumables.get(&what).copied().unwrap_or(0);
            if have < want {
                unmet.push(Unmet::Consumable { what, want, have });
            }
        }
        if let Some(want) = &self.device {
            unmet.extend(closest_device(want, node));
        }
        unmet
    }

    /// Whether the node meets every requirement.
    #[must_use]
    pub fn matches(&self, node: &NodeCaps) -> bool {
        self.unmet(node).is_empty()
    }
}

/// For a device key (`ios.device.<anything>`): `Some(Some(attribute))` for a known
/// attribute, `Some(None)` for one kbf does not know. `None` for every other key.
fn device_attribute(key: &str) -> Option<Option<String>> {
    let attribute = key.strip_prefix(IOS_DEVICE_KEY)?.strip_prefix('.')?;
    Some(
        DEVICE_ATTRIBUTES
            .contains(&attribute)
            .then(|| attribute.to_owned()),
    )
}

/// The attributes in `want` that the device with attributes `have` does not have.
fn device_unmet<'w>(
    want: &'w BTreeMap<String, String>,
    have: &BTreeMap<String, String>,
) -> impl Iterator<Item = (&'w str, &'w str)> {
    want.iter()
        .filter(|(attribute, value)| have.get(*attribute) != Some(*value))
        .map(|(attribute, value)| (attribute.as_str(), value.as_str()))
}

/// What a device request `want` lacks on `node`: [`Unmet::NoDevice`] if it reports no
/// device, nothing if one device has every attribute, else the attributes the closest
/// device lacks.
fn closest_device<'w>(want: &'w BTreeMap<String, String>, node: &NodeCaps) -> Vec<Unmet<'w>> {
    let Some((id, have, unmet)) = node
        .devices
        .iter()
        .map(|(id, have)| (id, have, device_unmet(want, have).collect::<Vec<_>>()))
        .min_by_key(|(_, _, unmet)| unmet.len())
    else {
        return vec![Unmet::NoDevice];
    };
    unmet
        .into_iter()
        .map(|(attribute, want)| Unmet::Device {
            attribute,
            want,
            device: id.clone(),
            have: have.get(attribute).cloned(),
        })
        .collect()
}

/// Sets `slot` if empty. `None` if it was already set.
fn set_once<T>(slot: &mut Option<T>, value: T) -> Option<()> {
    if slot.is_some() {
        return None;
    }
    *slot = Some(value);
    Some(())
}
