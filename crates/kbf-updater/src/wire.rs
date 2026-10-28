//! What an updater and the coordinator say to each other (`docs/design/updates.md`, section
//! 2): the heartbeat a host's updater sends, and the reply.
//!
//! The heartbeat holds only what the host knows about itself: its platform, the version it
//! runs, its health, the result of its last update, and the target of the update that is
//! running now. Who the host is (its node id and its role) is not in it: the coordinator
//! reads that from the host's certificate, so a heartbeat cannot speak for another host.
//! A reader ignores a field it does not know, so an updater newer than the coordinator can
//! add one.
//!
//! The reply is [`Reply::Nothing`] or [`Reply::Update`] with a [`Version`]. A version is a
//! validated string, never a URL or a path: the updater resolves the release itself and
//! checks its attestation.

use kbf_types::ReleasePlatform;
use serde::{Deserialize, Serialize};

use crate::version::Version;

/// The path an updater posts its heartbeat to.
pub const HEARTBEAT_PATH: &str = "/v1/heartbeat";

/// The most bytes of a heartbeat the coordinator reads.
pub const MAX_HEARTBEAT_BYTES: usize = 16 * 1024;

/// Whether the host serves what it should.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// The host's node passes its health check.
    Healthy,
    /// It does not, or the updater cannot tell.
    Unhealthy,
}

/// How an update ended on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The host runs the target and confirmed it healthy.
    Succeeded,
    /// The host is back on the version it ran before (or could not leave it).
    Failed,
}

/// The last update the host ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResult {
    /// The version it was told to run.
    pub target: Version,
    /// How it ended.
    pub outcome: Outcome,
}

/// One heartbeat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// The platform the host's release is built for.
    #[serde(with = "platform_text")]
    pub platform: ReleasePlatform,
    /// The version the host runs now.
    pub version: Version,
    /// Its health now.
    pub health: Health,
    /// The last update it ran, if it has run one since it started.
    #[serde(default)]
    pub last_update: Option<UpdateResult>,
    /// The target of the update running now: set from the start of the A/B to its end.
    #[serde(default)]
    pub updating: Option<Version>,
    /// What the host runs besides `kbf`, one entry per service. Absent from an updater that
    /// does not know services: the host is a `kbf`-only host, and the coordinator never
    /// names a service in its reply. Present (even empty) from one that does. `kbf` is never
    /// among them: its state is the fields above, and a heartbeat that lists it, lists a
    /// name twice, names a service that is no service name, or lists more than
    /// [`MAX_SERVICES`] is not read as a [`Heartbeat`]; the coordinator reads such a
    /// heartbeat with [`read_heartbeat`], which drops the list and keeps the rest.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "services_text"
    )]
    pub services: Option<Vec<ServiceBeat>>,
}

/// What a host says of one service besides `kbf`: the same four facts as the top level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceBeat {
    /// The service's name.
    pub name: String,
    /// The version of it the host runs now.
    pub version: Version,
    /// Its health now.
    pub health: Health,
    /// The last update of it the host ran, if it has run one since it started.
    #[serde(default)]
    pub last_update: Option<UpdateResult>,
    /// The target of the update of it running now.
    #[serde(default)]
    pub updating: Option<Version>,
}

impl ServiceBeat {
    /// This service's state as a heartbeat of a host on `platform`.
    #[must_use]
    pub fn as_heartbeat(&self, platform: ReleasePlatform) -> Heartbeat {
        Heartbeat {
            platform,
            version: self.version.clone(),
            health: self.health,
            last_update: self.last_update.clone(),
            updating: self.updating.clone(),
            services: None,
        }
    }
}

/// Whether `name` is a service name: lowercase letters, digits and `-`, not empty, and at
/// most [`MAX_SERVICE_NAME`] bytes.
#[must_use]
pub fn is_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVICE_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The most bytes of a service name.
pub const MAX_SERVICE_NAME: usize = 64;

/// The most services a heartbeat lists, and a services file defines. A host runs `kbf` and a
/// handful of daemons beside it; sixteen leaves room for all of them, and a list longer than
/// that is a mistake or an attack, not a farm.
pub const MAX_SERVICES: usize = 16;

/// The name the top-level fields of a heartbeat, and a reply without a `service`, are about.
pub const KBF_SERVICE: &str = "kbf";

fn services_text<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<ServiceBeat>>, D::Error> {
    use serde::de::Error;
    let Some(beats) = Option::<Vec<ServiceBeat>>::deserialize(d)? else {
        return Ok(None);
    };
    if beats.len() > MAX_SERVICES {
        return Err(D::Error::custom("too many services"));
    }
    for (i, beat) in beats.iter().enumerate() {
        if !is_service_name(&beat.name) {
            return Err(D::Error::custom("not a service name"));
        }
        if beat.name == KBF_SERVICE {
            return Err(D::Error::custom(
                "kbf is the top level of a heartbeat, not a service",
            ));
        }
        if beats[..i].iter().any(|b| b.name == beat.name) {
            return Err(D::Error::custom("a service is listed twice"));
        }
    }
    Ok(Some(beats))
}

/// A heartbeat as the coordinator reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatRead {
    /// The heartbeat. When its `services` list was dropped, `services` is `None`.
    pub beat: Heartbeat,
    /// Why the `services` list was dropped, if it was.
    pub services_dropped: Option<String>,
}

/// Reads a heartbeat, keeping a host whose `services` list is bad.
///
/// A bad list (see [`Heartbeat::services`]) is dropped and the rest is read, so the host is
/// answered as a `kbf`-only host and its `kbf` updates go on. Anything else that is wrong
/// with the heartbeat is an error, as for [`Heartbeat`].
///
/// # Errors
/// The strict read's error: the bytes are no heartbeat, with or without their `services`.
pub fn read_heartbeat(bytes: &[u8]) -> Result<HeartbeatRead, serde_json::Error> {
    match serde_json::from_slice::<Heartbeat>(bytes) {
        Ok(beat) => Ok(HeartbeatRead {
            beat,
            services_dropped: None,
        }),
        Err(why) => without_services(bytes)
            .map(|beat| HeartbeatRead {
                beat,
                services_dropped: Some(why.to_string()),
            })
            .ok_or(why),
    }
}

/// The heartbeat in `bytes` once its `services` key is removed, if there is one to remove
/// and what is left is a heartbeat.
fn without_services(bytes: &[u8]) -> Option<Heartbeat> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value.as_object_mut()?.remove("services")?;
    serde_json::from_value(value).ok()
}

/// The coordinator's answer to a heartbeat.
///
/// Read strictly: a field other than `reply` and `version`, a `version` on `nothing`, or an
/// `update` with none is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", try_from = "RawReply")]
pub enum Reply {
    /// Nothing to do.
    Nothing,
    /// Update to this version now.
    Update {
        /// The version, and nothing else of the release.
        version: Version,
    },
}

/// The reply to an updater that sent `services` in its heartbeat: [`Reply`], or an update
/// that names a service.
///
/// `Update` without a service is written and read exactly as [`Reply::Update`], and
/// `Nothing` as [`Reply::Nothing`]. An update with a `service` is a shape [`Reply`] refuses
/// (an updater that reads only [`Reply`] fails on the extra field), so the coordinator sends
/// it only to an updater that has sent `services`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", try_from = "RawServiceReply")]
pub enum ServiceReply {
    /// Nothing to do.
    Nothing,
    /// Update to this version now.
    Update {
        /// The service to update. `None` is `kbf`.
        #[serde(skip_serializing_if = "Option::is_none")]
        service: Option<String>,
        /// The version, and nothing else of the release.
        version: Version,
    },
}

impl From<Reply> for ServiceReply {
    fn from(reply: Reply) -> Self {
        match reply {
            Reply::Nothing => Self::Nothing,
            Reply::Update { version } => Self::Update {
                service: None,
                version,
            },
        }
    }
}

impl PartialEq<Reply> for ServiceReply {
    /// Whether this is that reply: the same answer about `kbf`.
    fn eq(&self, other: &Reply) -> bool {
        match (self, other) {
            (Self::Nothing, Reply::Nothing) => true,
            (
                Self::Update {
                    service: None,
                    version,
                },
                Reply::Update { version: other },
            ) => version == other,
            _ => false,
        }
    }
}

/// A service reply as it is read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServiceReply {
    reply: String,
    service: Option<String>,
    version: Option<Version>,
}

impl TryFrom<RawServiceReply> for ServiceReply {
    type Error = &'static str;

    fn try_from(raw: RawServiceReply) -> Result<Self, Self::Error> {
        if raw.service.as_deref().is_some_and(|s| !is_service_name(s)) {
            return Err("not a service name");
        }
        match (raw.reply.as_str(), raw.service, raw.version) {
            ("nothing", None, None) => Ok(Self::Nothing),
            ("update", service, Some(version)) => Ok(Self::Update { service, version }),
            ("nothing", _, _) => Err("a reply of nothing carries no service or version"),
            ("update", _, None) => Err("an update reply carries a version"),
            _ => Err("not a reply: nothing or update"),
        }
    }
}

/// A reply as it is read, before it is checked to be one of the two.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReply {
    reply: String,
    version: Option<Version>,
}

impl TryFrom<RawReply> for Reply {
    type Error = &'static str;

    fn try_from(raw: RawReply) -> Result<Self, Self::Error> {
        match (raw.reply.as_str(), raw.version) {
            ("nothing", None) => Ok(Self::Nothing),
            ("update", Some(version)) => Ok(Self::Update { version }),
            ("nothing", Some(_)) => Err("a reply of nothing carries no version"),
            ("update", None) => Err("an update reply carries a version"),
            _ => Err("not a reply: nothing or update"),
        }
    }
}

mod platform_text {
    use kbf_types::ReleasePlatform;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(platform: &ReleasePlatform, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(platform.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ReleasePlatform, D::Error> {
        let text = String::deserialize(d)?;
        ReleasePlatform::parse(&text)
            .ok_or_else(|| serde::de::Error::custom("not a platform kbf releases are built for"))
    }
}
