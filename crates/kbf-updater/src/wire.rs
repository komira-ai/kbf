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

use std::collections::BTreeMap;

use kbf_types::ReleasePlatform;
use serde::{Deserialize, Serialize};

use crate::version::Version;
pub use crate::wire_report::{
    Answer, Class, CommitState, DeferredReport, MAX_FEATURES, MAX_STEP_NAME, MAX_STEP_SECS,
    MAX_TOKEN, Phase, StepName, StepRun, Token,
};
use crate::wire_report::{features_text, is_false, lenient, or_default, step_secs_text};

/// The path an updater posts its heartbeat to.
pub const HEARTBEAT_PATH: &str = "/v1/heartbeat";

/// The most bytes of a heartbeat the coordinator reads.
///
/// A beat with every optional fact at its bounds, for `kbf` and sixteen services, can exceed
/// it (`wire_size_tests`); the agent trims optional facts first to keep a beat under it.
pub const MAX_HEARTBEAT_BYTES: usize = 16 * 1024;

/// Whether the host serves what it should.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// The host's node passes its health check.
    Healthy,
    /// It does not, or the updater cannot tell.
    Unhealthy,
    /// The service is declared on the host and has never registered. An agent sends it only
    /// to a coordinator that advertised `heartbeat-v2`: an older coordinator refuses the
    /// heartbeat.
    Absent,
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
    /// Where a failure happened. A value this reader does not know reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub phase: Option<Phase>,
    /// Whom a failure is blamed on. A value this reader does not know reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub class: Option<Class>,
    /// The check or cause of a failure, as a token. A value that is no token reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub reason: Option<Token>,
}

impl UpdateResult {
    /// A result with no phase, class or reason: what an updater without them reports.
    #[must_use]
    pub fn new(target: Version, outcome: Outcome) -> Self {
        Self {
            target,
            outcome,
            phase: None,
            class: None,
            reason: None,
        }
    }
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
    /// What this updater can use of the coordinator's side, by feature name: at most
    /// [`MAX_FEATURES`], each a lowercase name of letters, digits and `-`, none twice. Absent
    /// from an updater that announces nothing; the coordinator then sends no `features`.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "features_text"
    )]
    pub agent_features: Vec<String>,
    /// The step the update running now is in, and the try.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepRun>,
    /// Whether the update running now is past the hook's commit point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitState>,
    /// How many seconds each step of the last update took, by step name; at most
    /// [`MAX_STEP_SECS`] steps.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "step_secs_text"
    )]
    pub step_secs: BTreeMap<StepName, u64>,
    /// Serving, but not yet fully its own: the coordinator holds the host's slot.
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "or_default"
    )]
    pub promotion_pending: bool,
    /// An update the host declined for a reason about its moment, not the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredReport>,
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
    /// The step the update running now is in, and the try.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepRun>,
    /// Whether the update running now is past the hook's commit point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitState>,
    /// How many seconds each step of the last update took, by step name; at most
    /// [`MAX_STEP_SECS`] steps.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "step_secs_text"
    )]
    pub step_secs: BTreeMap<StepName, u64>,
    /// Serving, but not yet fully its own: the coordinator holds the host's slot.
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "or_default"
    )]
    pub promotion_pending: bool,
    /// An update the host declined for a reason about its moment, not the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredReport>,
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
            agent_features: Vec::new(),
            step: self.step.clone(),
            commit: self.commit,
            step_secs: self.step_secs.clone(),
            promotion_pending: self.promotion_pending,
            deferred: self.deferred.clone(),
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
    /// The heartbeat. A field that was dropped is `None` (or empty, for a list).
    pub beat: Heartbeat,
    /// Why the `services` list was dropped, if it was.
    pub services_dropped: Option<String>,
    /// Why one of the optional facts (`agent_features`, `step`, `commit`, `step_secs`,
    /// `promotion_pending`, `deferred`) was dropped, if one was.
    pub extras_dropped: Option<String>,
    /// The names of the facts `extras_dropped` is about, sorted, each once: `step`, `commit`,
    /// `step_secs`, `promotion_pending`, `deferred` or `agent_features`, whether the bad one
    /// was at the top level or in a service entry.
    pub extras_dropped_facts: Vec<&'static str>,
}

/// Reads a heartbeat, keeping a host whose `services` list, or one of its optional facts, is
/// bad.
///
/// A bad list (see [`Heartbeat::services`]) is dropped and the rest is read, so the host is
/// answered as a `kbf`-only host and its `kbf` updates go on. A bad optional fact is dropped
/// the same way, on its own: a host never loses its `kbf` updates to a field the coordinator
/// could do without. Anything else that is wrong with the heartbeat is an error, as for
/// [`Heartbeat`].
///
/// # Errors
/// The strict read's error: the bytes are no heartbeat, with or without the dropped fields.
pub fn read_heartbeat(bytes: &[u8]) -> Result<HeartbeatRead, serde_json::Error> {
    match serde_json::from_slice::<Heartbeat>(bytes) {
        Ok(beat) => Ok(HeartbeatRead {
            beat,
            services_dropped: None,
            extras_dropped: None,
            extras_dropped_facts: Vec::new(),
        }),
        Err(why) => {
            let Some(read) = without_droppable(bytes) else {
                return Err(why);
            };
            if read.services_dropped.is_none() && read.extras_dropped.is_none() {
                // Nothing the loose read dropped explains the refusal.
                return Err(why);
            }
            Ok(read)
        }
    }
}

/// A [`Heartbeat`] whose droppable values are not read yet: it reads every other field as
/// [`Heartbeat`] does, duplicate keys included.
#[derive(Deserialize)]
struct HeartbeatLoose {
    #[serde(with = "platform_text")]
    platform: ReleasePlatform,
    version: Version,
    health: Health,
    #[serde(default)]
    last_update: Option<UpdateResult>,
    #[serde(default)]
    updating: Option<Version>,
    #[serde(default)]
    services: Option<serde_json::Value>,
    #[serde(default)]
    agent_features: Option<serde_json::Value>,
    #[serde(default)]
    step: Option<serde_json::Value>,
    #[serde(default)]
    commit: Option<serde_json::Value>,
    #[serde(default)]
    step_secs: Option<serde_json::Value>,
    #[serde(default)]
    promotion_pending: Option<serde_json::Value>,
    #[serde(default)]
    deferred: Option<serde_json::Value>,
}

/// One droppable value read as `T`: its default, and the reason, when it is bad.
fn loose<T: Default + serde::de::DeserializeOwned>(
    name: &'static str,
    value: Option<serde_json::Value>,
    why: &mut Vec<(&'static str, String)>,
) -> T {
    let Some(value) = value else {
        return T::default();
    };
    serde_json::from_value(value).unwrap_or_else(|e| {
        why.push((name, format!("{name}: {e}")));
        T::default()
    })
}

/// Removes from each entry of the `services` list in `value` every new fact that does not
/// read, and records it in `why` as `services[<name>].<fact>: <error>`. The structural rules
/// of the list (names, duplicates, `kbf`, the count) are left to [`services_text`], so a bad
/// fact costs that fact and never the host's services.
fn scrub_service_facts(value: &mut serde_json::Value, why: &mut Vec<(&'static str, String)>) {
    let Some(entries) = value.as_array_mut() else {
        return;
    };
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        for fact in [
            "step",
            "commit",
            "step_secs",
            "promotion_pending",
            "deferred",
        ] {
            let Some(fact_value) = object.get(fact).cloned() else {
                continue;
            };
            let read = match fact {
                "step" => serde_json::from_value::<Option<StepRun>>(fact_value).err(),
                "commit" => serde_json::from_value::<Option<CommitState>>(fact_value).err(),
                "step_secs" => step_secs_text(fact_value).err(),
                "promotion_pending" => or_default::<_, bool>(fact_value).err(),
                _ => serde_json::from_value::<Option<DeferredReport>>(fact_value).err(),
            };
            if let Some(error) = read {
                object.remove(fact);
                why.push((fact, format!("services[{name}].{fact}: {error}")));
            }
        }
    }
}

/// The heartbeat in `bytes` with each bad droppable value dropped, if what is left is a
/// heartbeat.
fn without_droppable(bytes: &[u8]) -> Option<HeartbeatRead> {
    let rest: HeartbeatLoose = serde_json::from_slice(bytes).ok()?;
    let mut extras = Vec::new();
    let mut services_why = Vec::new();
    let services = rest.services.and_then(|mut value| {
        scrub_service_facts(&mut value, &mut extras);
        services_text(value)
            .map_err(|e| services_why.push(e.to_string()))
            .ok()
            .flatten()
    });
    let beat = Heartbeat {
        platform: rest.platform,
        version: rest.version,
        health: rest.health,
        last_update: rest.last_update,
        updating: rest.updating,
        services,
        agent_features: features_text(rest.agent_features.unwrap_or_default())
            .map_err(|e| extras.push(("agent_features", format!("agent_features: {e}"))))
            .unwrap_or_default(),
        step: loose("step", rest.step, &mut extras),
        commit: loose("commit", rest.commit, &mut extras),
        step_secs: step_secs_text(rest.step_secs.unwrap_or_default())
            .map_err(|e| extras.push(("step_secs", format!("step_secs: {e}"))))
            .unwrap_or_default(),
        promotion_pending: loose("promotion_pending", rest.promotion_pending, &mut extras),
        deferred: loose("deferred", rest.deferred, &mut extras),
    };
    let mut facts: Vec<&'static str> = extras.iter().map(|(fact, _)| *fact).collect();
    facts.sort_unstable();
    facts.dedup();
    let why: Vec<&str> = extras.iter().map(|(_, why)| why.as_str()).collect();
    Some(HeartbeatRead {
        beat,
        services_dropped: (!services_why.is_empty()).then(|| services_why.join("; ")),
        extras_dropped: (!why.is_empty()).then(|| why.join("; ")),
        extras_dropped_facts: facts,
    })
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

impl ServiceReply {
    /// The reply the three fields of a service reply say, if they say one.
    pub(crate) fn from_parts(
        reply: String,
        service: Option<String>,
        version: Option<Version>,
    ) -> Result<Self, &'static str> {
        if service.as_deref().is_some_and(|s| !is_service_name(s)) {
            return Err("not a service name");
        }
        match (reply.as_str(), service, version) {
            ("nothing", None, None) => Ok(Self::Nothing),
            ("update", service, Some(version)) => Ok(Self::Update { service, version }),
            ("nothing", _, _) => Err("a reply of nothing carries no service or version"),
            ("update", _, None) => Err("an update reply carries a version"),
            _ => Err("not a reply: nothing or update"),
        }
    }
}

impl TryFrom<RawServiceReply> for ServiceReply {
    type Error = &'static str;

    fn try_from(raw: RawServiceReply) -> Result<Self, Self::Error> {
        Self::from_parts(raw.reply, raw.service, raw.version)
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
