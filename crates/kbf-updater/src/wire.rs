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
