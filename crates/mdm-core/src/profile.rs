//! The two profiles the kit hands Apple's side: the enrollment profile a Mac installs
//! ([`enrollment`], a `.mobileconfig` built with the `plist` crate) and the Automated
//! Device Enrollment profile NanoDEP defines at Apple ([`AdeProfile`]).
//!
//! The enrollment profile holds two payloads: a SCEP payload, through which the Mac
//! gets its identity certificate from `https://<MDM_DNS_NAME>/scep` with the SCEP
//! challenge, and the MDM payload, which names that certificate (its
//! `IdentityCertificateUUID` is the SCEP payload's `PayloadUUID`), the server
//! `https://<MDM_DNS_NAME>/mdm` and the push topic. Its identifiers are
//! `MDM_PROFILE_ID` (`.scep` and `.mdm` for the payloads). Its three payload UUIDs are
//! derived from `MDM_PROFILE_ID` ([`payload_uuids`]), so a render with the same settings
//! is the same profile, not a second one, and two sites never share a payload UUID.
//!
//! The ADE profile is built from the settings, and [`AdeProfile::parse`] reads one back
//! and refuses an unknown key and a profile a device could leave: one that is not
//! supervised, not mandatory, removable, or that sends the device anywhere but
//! `https://<MDM_DNS_NAME>/enroll`.

use std::fmt;
use std::io::Write;

use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};

use crate::config::MdmConfig;
use crate::sha1::sha1;

/// The namespace of the payload UUIDs: a fixed UUID of this kit, the first input of the
/// version 5 (SHA-1, RFC 4122) derivation in [`payload_uuids`].
const UUID_NAMESPACE: [u8; 16] = [
    0x6f, 0x1d, 0x2a, 0x4e, 0x8c, 0x3b, 0x4e, 0x57, 0x9a, 0x10, 0x5d, 0x2b, 0x7c, 0x9e, 0x4f, 0x13,
];
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

/// The three payload UUIDs of the enrollment profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadUuids {
    /// The profile itself.
    pub profile: String,
    /// The SCEP payload, which the MDM payload names as its identity.
    pub scep: String,
    /// The MDM payload.
    pub mdm: String,
}

/// The payload UUIDs for the profile identifier `profile_id`: for each role (`profile`,
/// `scep`, `mdm`) the version 5 UUID (RFC 4122, SHA-1) of the name
/// `<profile_id>/<role>` in this kit's namespace, written in upper case. The same
/// identifier always gives the same UUIDs, so a re-render is the same profile, and two
/// sites with different identifiers share none.
pub fn payload_uuids(profile_id: &str) -> PayloadUuids {
    let uuid = |role: &str| uuid_v5(&UUID_NAMESPACE, format!("{profile_id}/{role}").as_bytes());
    PayloadUuids {
        profile: uuid("profile"),
        scep: uuid("scep"),
        mdm: uuid("mdm"),
    }
}

/// The version 5 UUID (RFC 4122: SHA-1 of the namespace and the name) as 36 upper-case
/// characters.
pub fn uuid_v5(namespace: &[u8; 16], name: &[u8]) -> String {
    let mut input = namespace.to_vec();
    input.extend_from_slice(name);
    let mut b = [0u8; 16];
    b.copy_from_slice(&sha1(&input)[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode_upper(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// The SCEP challenge the enrollment profile carries: letters, digits, `_` and `-`,
/// at least one. Its `Debug` hides it and it has no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Challenge(String);

impl Challenge {
    /// `text` as a challenge, or why it is not one.
    pub fn new(text: &str) -> Result<Challenge, ProfileError> {
        let ok = !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !ok {
            return Err(ProfileError::Challenge);
        }
        Ok(Challenge(text.to_owned()))
    }

    /// The value, for the one place it goes (the profile).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Challenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Challenge(..)")
    }
}

/// An MDM push topic, `com.apple.mgmt.External.<uuid>`: the subject UID of an APNs MDM
/// push certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic(String);

impl Topic {
    /// `text` as a topic, or why it is not one.
    pub fn new(text: &str) -> Result<Topic, ProfileError> {
        let uuid = text.strip_prefix("com.apple.mgmt.External.");
        // 8-4-4-4-12 hexadecimal digits.
        let ok = uuid.is_some_and(|u| {
            let groups: Vec<&str> = u.split('-').collect();
            groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
                && groups
                    .iter()
                    .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
        });
        if !ok {
            return Err(ProfileError::Topic(text.to_owned()));
        }
        Ok(Topic(text.to_owned()))
    }

    /// The topic as text.
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
    /// Not a SCEP challenge.
    #[error("the SCEP challenge is empty or holds a character other than letters, digits, _ and -")]
    Challenge,
    /// The property list could not be written.
    #[error("cannot write the enrollment profile: {0}")]
    Write(String),
    /// Not a push topic.
    #[error("topic {0:?} is not com.apple.mgmt.External.<uuid>")]
    Topic(String),
    /// Not the JSON the kit reads.
    #[error("the ADE profile is not the JSON this kit reads: {0}")]
    Json(String),
    /// Sends devices to another server.
    #[error("the ADE profile sends devices to {got:?}, not {want:?}")]
    Url {
        /// The URL in the profile.
        got: String,
        /// The URL it must hold.
        want: String,
    },
    /// Not supervised.
    #[error(
        "the ADE profile is not supervised (is_supervised): an unsupervised Mac takes no erase or lock"
    )]
    NotSupervised,
    /// Skippable.
    #[error(
        "the ADE profile is not mandatory (is_mandatory): Setup Assistant would let a person skip it"
    )]
    NotMandatory,
    /// The MDM can be removed from the Mac.
    #[error(
        "the ADE profile is removable (is_mdm_removable): a person could remove the MDM from the Mac"
    )]
    Removable,
}

/// The enrollment profile for the MDM at `MDM_DNS_NAME`, named `MDM_PROFILE_ID` and
/// `MDM_PROFILE_DISPLAY_NAME`, as the bytes of an XML property list ending in a newline.
pub fn enrollment(
    c: &MdmConfig,
    topic: &Topic,
    challenge: &Challenge,
) -> Result<Vec<u8>, ProfileError> {
    let mut out = Vec::new();
    enrollment_to(c, topic, challenge, &mut out)?;
    Ok(out)
}

impl From<plist::Error> for ProfileError {
    fn from(e: plist::Error) -> ProfileError {
        ProfileError::Write(e.to_string())
    }
}

impl From<std::io::Error> for ProfileError {
    fn from(e: std::io::Error) -> ProfileError {
        ProfileError::Write(e.to_string())
    }
}

/// [`enrollment`], written to `out`.
pub fn enrollment_to<W: Write>(
    c: &MdmConfig,
    topic: &Topic,
    challenge: &Challenge,
    out: &mut W,
) -> Result<(), ProfileError> {
    let uuids = payload_uuids(&c.profile_id);
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
        ("PayloadUUID", text(&uuids.scep)),
        ("PayloadVersion", Value::Integer(1.into())),
    ]);
    let capabilities = CAPABILITIES.iter().map(|c| text(c)).collect();
    let mdm = dict([
        ("AccessRights", Value::Integer(ACCESS_RIGHTS.into())),
        ("CheckOutWhenRemoved", Value::Boolean(true)),
        ("IdentityCertificateUUID", text(&uuids.scep)),
        ("PayloadIdentifier", text(&format!("{id}.mdm"))),
        ("PayloadType", text("com.apple.mdm")),
        ("PayloadUUID", text(&uuids.mdm)),
        ("PayloadVersion", Value::Integer(1.into())),
        ("ServerCapabilities", Value::Array(capabilities)),
        ("ServerURL", text(&format!("https://{dns_name}/mdm"))),
        ("SignMessage", Value::Boolean(true)),
        ("Topic", text(topic.as_str())),
    ]);
    let profile = dict([
        ("PayloadContent", Value::Array(vec![scep, mdm])),
        ("PayloadDisplayName", text(&c.profile_display_name)),
        ("PayloadIdentifier", text(id)),
        ("PayloadOrganization", text(org)),
        ("PayloadType", text("Configuration")),
        ("PayloadUUID", text(&uuids.profile)),
        ("PayloadVersion", Value::Integer(1.into())),
    ]);
    profile.to_writer_xml(&mut *out)?;
    out.write_all(b"\n")?;
    Ok(())
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
    /// The profile's name at Apple.
    pub profile_name: String,
    /// Where a device is sent to enroll.
    pub url: String,
    /// Whether the device is supervised.
    pub is_supervised: bool,
    /// Whether a person must take the profile.
    pub is_mandatory: bool,
    /// Whether a person may remove the MDM.
    pub is_mdm_removable: bool,
    /// Whether Setup Assistant waits for the MDM to finish configuring the device.
    pub await_device_configured: bool,
    /// Whether Setup Assistant advances by itself.
    pub auto_advance_setup: bool,
    /// The `org_magic`.
    pub org_magic: String,
    /// The language.
    pub language: String,
    /// The region.
    pub region: String,
    /// Certificates the device trusts during enrollment.
    pub anchor_certs: Vec<String>,
    /// The Setup Assistant panes skipped.
    pub skip_setup_items: Vec<String>,
}

impl AdeProfile {
    /// The profile for the settings `c`: supervised, mandatory, not removable, sending
    /// devices to `https://<MDM_DNS_NAME>/enroll`.
    pub fn from_config(c: &MdmConfig) -> AdeProfile {
        AdeProfile {
            profile_name: c.ade.profile_name.clone(),
            url: format!("https://{}/enroll", c.dns_name),
            is_supervised: true,
            is_mandatory: true,
            is_mdm_removable: false,
            await_device_configured: false,
            auto_advance_setup: false,
            org_magic: c.ade.org_magic.clone(),
            language: c.ade.language.clone(),
            region: c.ade.region.clone(),
            anchor_certs: Vec::new(),
            skip_setup_items: c.ade.skip_items.clone(),
        }
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
