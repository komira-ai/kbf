//! Reading a node report (the `(key, value)` entries a daemon sends in `Hello`) as the
//! [`NodeCaps`] requests are matched against.
//!
//! | Report entry | Becomes |
//! |---|---|
//! | `arch` (required, once) | the CPU's architecture |
//! | `cpu.features` (repeated) | the CPU's features; the ISA level follows from them |
//! | `cpus`, `mem_gib`, `nvme_gib`, `gpu` (once each) | countable capacity |
//! | `os`, `os_image`, `cpu.model`, `page_size`, `gpu.kind` (once each) | exact values |
//! | `gpu.cores`, `gpu.metal_family` (a Mac reports them) | not read: a request cannot name them yet |
//! | `xcode` (repeated: one per installed build) | a set, matched by membership |
//! | `vm.image` (repeated: one per image, `<name>@sha256:<64 hex>`) | a set of the `sha256:<hex>` digests, matched by membership |
//! | `vm.slots`, `vm.max_cpus`, `vm.max_mem_gib` (once each) | checked to be whole numbers, not matched on (report-only) |
//! | `drivers` (repeated: one per driver the daemon runs) | the drivers; placement reads them for the lease kind, no request key names them |
//! | `ios.device` (repeated: one per ready device, comma-separated `k=v` fields with an `id`; a value may hold commas, never `=`) | the devices, by id, each with its other attributes; a device request matches one of them |
//!
//! Every other entry (`isa_level`, a `label.<k>` an older daemon still sends, ...) is not
//! matched on and is skipped.
//! The reported `isa_level` list is not read: the level is computed from the features,
//! so it can never disagree with them.

use std::collections::{BTreeMap, BTreeSet};

use crate::cpu::{Arch, CpuCaps, UnknownArch};
use crate::matching::{
    Consumable, IOS_DEVICE_KEY, NodeCaps, REPORT_ONLY_KEYS, is_exact_key, is_member_key,
    member_token,
};

/// Why a node report does not describe a node.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    /// The report has no `arch` entry.
    #[error("the node report has no \"arch\" entry")]
    NoArch,
    /// The `arch` entry names no architecture kbf knows.
    #[error(transparent)]
    UnknownArch(#[from] UnknownArch),
    /// An entry that takes one value appears more than once.
    #[error("node report entry {0:?} appears more than once")]
    Repeated(String),
    /// A `vm.image` entry without a digest.
    #[error(
        "node report entry vm.image={0:?} names no digest; the form is \
         <name>@sha256:<64 lowercase hex digits>"
    )]
    NoImageDigest(String),
    /// An `ios.device` entry that is not comma-separated `k=v` fields, each key once,
    /// every key and value non-empty, no `=` in a value, with an `id`.
    #[error(
        "node report entry ios.device={0:?} is not a comma-separated list of k=v fields \
         with an id"
    )]
    BadDevice(String),
    /// Two `ios.device` entries name one device id.
    #[error("node report names iOS device {0:?} twice")]
    RepeatedDevice(String),
    /// A countable entry is not a whole number.
    #[error("node report entry {key}={value:?} is not a whole number")]
    NotANumber {
        /// The entry's key.
        key: String,
        /// Its value.
        value: String,
    },
}

impl NodeCaps {
    /// The capabilities a node report describes; see the module table.
    ///
    /// # Errors
    /// No `arch`, an unknown one, a single-valued entry repeated, a countable or
    /// report-only entry that is not a whole number, a `vm.image` entry without a
    /// digest, or an `ios.device` entry that is malformed or names a device id again.
    pub fn from_report<'r, I>(entries: I) -> Result<Self, ReportError>
    where
        I: IntoIterator<Item = (&'r str, &'r str)>,
    {
        Self::from_each(&mut entries.into_iter())
    }

    /// [`Self::from_report`], compiled once rather than once per caller's iterator type.
    fn from_each<'r>(
        entries: &mut dyn Iterator<Item = (&'r str, &'r str)>,
    ) -> Result<Self, ReportError> {
        let mut arch = None;
        let mut features = BTreeSet::new();
        let mut exact = BTreeMap::new();
        let mut members: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut consumables = BTreeMap::new();
        let mut report_only = BTreeSet::new();
        let number = |key: &str, value: &str| {
            value.parse::<u64>().map_err(|_| ReportError::NotANumber {
                key: key.to_owned(),
                value: value.to_owned(),
            })
        };
        let mut drivers = BTreeSet::new();
        let mut devices = BTreeMap::new();
        let repeated = |key: &str| ReportError::Repeated(key.to_owned());
        for (key, value) in entries {
            if key == "arch" {
                if arch.replace(value).is_some() {
                    return Err(repeated(key));
                }
            } else if key == "cpu.features" {
                features.insert(value.to_owned());
            } else if key == "drivers" {
                drivers.insert(value.to_owned());
            } else if key == IOS_DEVICE_KEY {
                let (id, attributes) =
                    device(value).ok_or_else(|| ReportError::BadDevice(value.to_owned()))?;
                if devices.contains_key(&id) {
                    return Err(ReportError::RepeatedDevice(id));
                }
                devices.insert(id, attributes);
            } else if is_member_key(key) {
                let token = member_token(key, value)
                    .ok_or_else(|| ReportError::NoImageDigest(value.to_owned()))?;
                members
                    .entry(key.to_owned())
                    .or_default()
                    .insert(token.to_owned());
            } else if REPORT_ONLY_KEYS.contains(&key) {
                number(key, value)?;
                if !report_only.insert(key) {
                    return Err(repeated(key));
                }
            } else if let Some(what) = Consumable::from_name(key) {
                let amount = number(key, value)?;
                if consumables.insert(what, amount).is_some() {
                    return Err(repeated(key));
                }
            } else if is_exact_key(key) && exact.insert(key.to_owned(), value.to_owned()).is_some()
            {
                return Err(repeated(key));
            }
        }
        let arch: Arch = arch.ok_or(ReportError::NoArch)?.parse()?;
        Ok(Self {
            cpu: CpuCaps::new(arch, features),
            exact,
            members,
            consumables,
            drivers,
            devices,
        })
    }
}

/// An `ios.device` entry's id and its other fields, or `None` if it is malformed (see
/// [`ReportError::BadDevice`]). A field is `<key>=<value>`, fields are separated by
/// commas, and a value may hold commas but no `=`: Apple's model identifiers do
/// (`product_type=iPhone17,3`), so a comma starts a new field only where `=` follows
/// before the next comma. Fields kbf does not match on are kept: a request can name
/// only [`crate::DEVICE_ATTRIBUTES`], so they never decide a match.
fn device(value: &str) -> Option<(String, BTreeMap<String, String>)> {
    let mut fields: Vec<(&str, String)> = Vec::new();
    for piece in value.split(',') {
        match piece.split_once('=') {
            Some((key, value)) if !key.is_empty() && !value.contains('=') => {
                fields.push((key, value.to_owned()));
            }
            Some(_) => return None,
            // A comma inside the value of the field before.
            None => fields.last_mut()?.1.extend([",", piece]),
        }
    }
    let mut map = BTreeMap::new();
    for (key, value) in fields {
        if value.is_empty() || map.insert(key.to_owned(), value).is_some() {
            return None;
        }
    }
    let id = map.remove("id")?;
    Some((id, map))
}
