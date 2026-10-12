//! The two profiles the kit hands Apple's side: the enrollment profile a Mac installs
//! ([`enrollment`], a `.mobileconfig` built with the `plist` crate) and the Automated
//! Device Enrollment profile NanoDEP defines at Apple ([`AdeProfile`]).
//!
//! The enrollment profile holds two payloads: a SCEP payload, through which the Mac
//! gets its identity certificate from `https://<MDM_DNS_NAME>/scep` with the SCEP
//! challenge, and the MDM payload, which names that certificate (its
//! `IdentityCertificateUUID` is the SCEP payload's `PayloadUUID`), the server
//! `https://<MDM_DNS_NAME>/mdm` and the push topic. Its identifiers are
//! `MDM_PROFILE_ID` (`.scep` and `.mdm` for the payloads) and its payload UUIDs are
//! fixed, so a render with the same settings is the same profile, not a second one.
//!
//! The ADE profile is rendered from `templates/mdm/dep_profile.json`, then read back
//! into [`AdeProfile`], which refuses an unknown key and a profile a device could
//! leave: one that is not supervised, not mandatory, removable, or that sends the
//! device anywhere but `https://<MDM_DNS_NAME>/enroll`.

use std::fmt;

use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};

use super::config::MdmConfig;
use super::secret::Secret;
use crate::template::{self, TemplateError, Values};

/// The ADE profile's template.
pub const ADE_TEMPLATE: &str = include_str!("../../templates/mdm/dep_profile.json");

/// The SCEP payload's UUID, which the MDM payload names as its identity.
const SCEP_UUID: &str = "071DB849-C11F-4A24-B1E8-69113460B678";
const MDM_UUID: &str = "25B0DB54-2EDA-4965-B430-24DB8BFC06DD";
const PROFILE_UUID: &str = "5F652B4C-C5C9-4D69-B44E-493CB00C698E";
/// Every MDM right (`AccessRights`, bits 1 to 4096).
const ACCESS_RIGHTS: i64 = 8191;
/// The SCEP key: RSA 2048, for signing and encryption (`Key Usage` 1 | 4).
const KEY_SIZE: i64 = 2048;
const KEY_USAGE: i64 = 5;
const CAPABILITIES: [&str; 3] = [
    "com.apple.mdm.per-user-connections",
    "com.apple.mdm.bootstraptoken",
    "com.apple.mdm.token",
];

/// An MDM push topic, `com.apple.mgmt.External.<uuid>`: the subject UID of an APNs MDM
/// push certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic(String);

impl Topic {
    pub fn new(text: &str) -> Result<Topic, ProfileError> {
        let uuid = text.strip_prefix("com.apple.mgmt.External.");
        let ok = uuid
            .is_some_and(|u| u.len() == 36 && u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        if !ok {
            return Err(ProfileError::Topic(text.to_owned()));
        }
        Ok(Topic(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a profile, or a part of one, was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("topic {0:?} is not com.apple.mgmt.External.<uuid>")]
    Topic(String),
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error("the ADE profile is not the JSON this kit reads: {0}")]
    Json(String),
    #[error("the ADE profile sends devices to {got:?}, not {want:?}")]
    Url { got: String, want: String },
    #[error(
        "the ADE profile is not supervised (is_supervised): an unsupervised Mac takes no erase or lock"
    )]
    NotSupervised,
    #[error(
        "the ADE profile is not mandatory (is_mandatory): Setup Assistant would let a person skip it"
    )]
    NotMandatory,
    #[error(
        "the ADE profile is removable (is_mdm_removable): a person could remove the MDM from the Mac"
    )]
    Removable,
}

/// The enrollment profile for the MDM at `MDM_DNS_NAME`, named `MDM_PROFILE_ID` and
/// `MDM_ORG`, as the bytes of an XML property list.
pub fn enrollment(c: &MdmConfig, topic: &Topic, challenge: &Secret) -> Vec<u8> {
    let (dns_name, id, org) = (&c.dns_name, &c.profile_id, &c.org);
    let scep = dict([
        ("Key Type", text("RSA")),
        ("Challenge", text(challenge.expose())),
        ("Key Usage", Value::Integer(KEY_USAGE.into())),
        ("Keysize", Value::Integer(KEY_SIZE.into())),
        ("URL", text(&format!("https://{dns_name}/scep"))),
    ]);
    let scep = dict([
        ("PayloadContent", scep),
        ("PayloadIdentifier", text(&format!("{id}.scep"))),
        ("PayloadType", text("com.apple.security.scep")),
        ("PayloadUUID", text(SCEP_UUID)),
        ("PayloadVersion", Value::Integer(1.into())),
    ]);
    let capabilities = CAPABILITIES.iter().map(|c| text(c)).collect();
    let mdm = dict([
        ("AccessRights", Value::Integer(ACCESS_RIGHTS.into())),
        ("CheckOutWhenRemoved", Value::Boolean(true)),
        ("IdentityCertificateUUID", text(SCEP_UUID)),
        ("PayloadIdentifier", text(&format!("{id}.mdm"))),
        ("PayloadType", text("com.apple.mdm")),
        ("PayloadUUID", text(MDM_UUID)),
        ("PayloadVersion", Value::Integer(1.into())),
        ("ServerCapabilities", Value::Array(capabilities)),
        ("ServerURL", text(&format!("https://{dns_name}/mdm"))),
        ("SignMessage", Value::Boolean(true)),
        ("Topic", text(topic.as_str())),
    ]);
    let profile = dict([
        ("PayloadContent", Value::Array(vec![scep, mdm])),
        ("PayloadDisplayName", text(&format!("{org} build farm MDM"))),
        ("PayloadIdentifier", text(id)),
        ("PayloadOrganization", text(org)),
        ("PayloadType", text("Configuration")),
        ("PayloadUUID", text(PROFILE_UUID)),
        ("PayloadVersion", Value::Integer(1.into())),
    ]);
    let mut out = Vec::new();
    profile
        .to_writer_xml(&mut out)
        .expect("a property list of strings, numbers and booleans writes to memory");
    out.push(b'\n');
    out
}

fn text(s: &str) -> Value {
    Value::String(s.to_owned())
}

fn dict<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut d = Dictionary::new();
    for (key, value) in pairs {
        d.insert(key.to_owned(), value);
    }
    Value::Dictionary(d)
}

/// The Automated Device Enrollment profile, as Apple's `define profile` call takes it.
/// The fields are in the order they are sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdeProfile {
    pub profile_name: String,
    pub url: String,
    pub is_supervised: bool,
    pub is_mandatory: bool,
    pub is_mdm_removable: bool,
    pub await_device_configured: bool,
    pub auto_advance_setup: bool,
    pub org_magic: String,
    pub language: String,
    pub region: String,
    pub anchor_certs: Vec<String>,
    pub skip_setup_items: Vec<String>,
}

impl AdeProfile {
    /// The kit's template, rendered with the settings and checked.
    pub fn render(c: &MdmConfig) -> Result<AdeProfile, ProfileError> {
        // The organisation goes inside a JSON string: escaped as one.
        let org = serde_json::to_string(&c.org).expect("a string serializes");
        let org = org[1..org.len() - 1].to_owned();
        let values: Values = [
            ("ORG", template::Value::Text(org)),
            ("DNS_NAME", template::Value::Text(c.dns_name.clone())),
        ]
        .into_iter()
        .collect();
        let text = template::render("dep_profile.json", ADE_TEMPLATE, &values)?;
        AdeProfile::parse(&text, &c.dns_name)
    }

    /// `text` read as a profile for the MDM at `dns_name`, refused unless a device it
    /// enrolls is supervised, cannot skip it, cannot remove the MDM, and is sent to
    /// this MDM.
    pub fn parse(text: &str, dns_name: &str) -> Result<AdeProfile, ProfileError> {
        let p: AdeProfile =
            serde_json::from_str(text).map_err(|e| ProfileError::Json(e.to_string()))?;
        let want = format!("https://{dns_name}/enroll");
        if p.url != want {
            return Err(ProfileError::Url { got: p.url, want });
        }
        if !p.is_supervised {
            return Err(ProfileError::NotSupervised);
        }
        if !p.is_mandatory {
            return Err(ProfileError::NotMandatory);
        }
        if p.is_mdm_removable {
            return Err(ProfileError::Removable);
        }
        Ok(p)
    }

    /// The bytes sent to Apple: pretty JSON in field order, ending in a newline.
    pub fn to_json(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("a profile serializes");
        out.push(b'\n');
        out
    }
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
