//! The write tier's health as a storage node's heartbeat carries it (`kbf_proto` map.proto,
//! `HeartbeatRequest` fields 17 to 23) and `GET /v1/storage` shows it.
//!
//! Every value is optional and `None` is unknown, never zero: an older node, a node with no
//! write tier and a node that has not heartbeated since it registered all send nothing.
//! The numbers inform an operator and decide nothing, so a value that cannot be a count of
//! anything real ([`MAX_REPORTED`]) is dropped to unknown on receipt rather than shown.

use kbf_front::TierHealth;
use kbf_proto::kbf::map::v1 as pb;
use serde::Serialize;

/// The largest value shown: 2^53 - 1, the largest integer a JSON number carries exactly.
/// A larger value is a node's bug or a forged beat, and is read as unknown.
pub const MAX_REPORTED: u64 = (1 << 53) - 1;

/// What one heartbeat says of the node's write tier. [`Vitals::default`] is a node that says
/// nothing: every value unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Vitals {
    /// Committed bytes of every block not yet flushed to the cold store.
    pub unflushed_bytes: Option<u64>,
    /// The blocks holding them.
    pub unflushed_blocks: Option<u64>,
    /// The age in milliseconds of the oldest such block (0: there is none).
    pub oldest_unflushed_ms: Option<u64>,
    /// The tier's budget in bytes.
    pub budget_bytes: Option<u64>,
    /// The bytes counted against the budget.
    pub used_bytes: Option<u64>,
    /// Flush attempts that failed since the node started.
    pub flush_failures: Option<u64>,
    /// Objects whose key in the cold store held another object, since the node started.
    pub flush_conflicts: Option<u64>,
}

/// `value` unless it is above [`MAX_REPORTED`].
const fn bounded(value: Option<u64>) -> Option<u64> {
    match value {
        Some(v) if v > MAX_REPORTED => None,
        other => other,
    }
}

impl Vitals {
    /// A node that says nothing: every value unknown.
    pub const NONE: Self = Self {
        unflushed_bytes: None,
        unflushed_blocks: None,
        oldest_unflushed_ms: None,
        budget_bytes: None,
        used_bytes: None,
        flush_failures: None,
        flush_conflicts: None,
    };

    /// What `beat` says, each value bounded.
    #[must_use]
    pub fn from_wire(beat: &pb::HeartbeatRequest) -> Self {
        Self {
            unflushed_bytes: bounded(beat.unflushed_bytes),
            unflushed_blocks: bounded(beat.unflushed_blocks),
            oldest_unflushed_ms: bounded(beat.oldest_unflushed_ms),
            budget_bytes: bounded(beat.budget_bytes),
            used_bytes: bounded(beat.used_bytes),
            flush_failures: bounded(beat.flush_failures),
            flush_conflicts: bounded(beat.flush_conflicts),
        }
    }

    /// The tier's health `health` (`None`: no tier, so nothing to say).
    #[must_use]
    pub fn from_tier(health: Option<TierHealth>) -> Self {
        let Some(h) = health else {
            return Self::default();
        };
        let millis = h
            .oldest_unflushed
            .map_or(0, |age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
        Self {
            unflushed_bytes: Some(h.unflushed_bytes),
            unflushed_blocks: Some(h.unflushed_blocks),
            oldest_unflushed_ms: Some(millis),
            budget_bytes: Some(h.budget_bytes),
            used_bytes: Some(h.used_bytes),
            flush_failures: Some(h.flush_failures),
            flush_conflicts: Some(h.flush_conflicts),
        }
    }

    /// `beat` with these values set.
    #[must_use]
    pub fn put_on(self, mut beat: pb::HeartbeatRequest) -> pb::HeartbeatRequest {
        beat.unflushed_bytes = self.unflushed_bytes;
        beat.unflushed_blocks = self.unflushed_blocks;
        beat.oldest_unflushed_ms = self.oldest_unflushed_ms;
        beat.budget_bytes = self.budget_bytes;
        beat.used_bytes = self.used_bytes;
        beat.flush_failures = self.flush_failures;
        beat.flush_conflicts = self.flush_conflicts;
        beat
    }
}

/// [`Vitals`] as stored in a member row.
#[derive(Clone, Copy, PartialEq, prost::Message)]
pub(crate) struct StoredVitals {
    #[prost(uint64, optional, tag = "1")]
    unflushed_bytes: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    unflushed_blocks: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    oldest_unflushed_ms: Option<u64>,
    #[prost(uint64, optional, tag = "4")]
    budget_bytes: Option<u64>,
    #[prost(uint64, optional, tag = "5")]
    used_bytes: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    flush_failures: Option<u64>,
    #[prost(uint64, optional, tag = "7")]
    flush_conflicts: Option<u64>,
}

impl From<Vitals> for StoredVitals {
    fn from(v: Vitals) -> Self {
        Self {
            unflushed_bytes: v.unflushed_bytes,
            unflushed_blocks: v.unflushed_blocks,
            oldest_unflushed_ms: v.oldest_unflushed_ms,
            budget_bytes: v.budget_bytes,
            used_bytes: v.used_bytes,
            flush_failures: v.flush_failures,
            flush_conflicts: v.flush_conflicts,
        }
    }
}

impl From<StoredVitals> for Vitals {
    fn from(s: StoredVitals) -> Self {
        Self {
            unflushed_bytes: s.unflushed_bytes,
            unflushed_blocks: s.unflushed_blocks,
            oldest_unflushed_ms: s.oldest_unflushed_ms,
            budget_bytes: s.budget_bytes,
            used_bytes: s.used_bytes,
            flush_failures: s.flush_failures,
            flush_conflicts: s.flush_conflicts,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use prost::Message as _;

    use super::*;

    fn health() -> TierHealth {
        TierHealth {
            budget_bytes: 1000,
            used_bytes: 400,
            unflushed_bytes: 300,
            unflushed_blocks: 2,
            oldest_unflushed: Some(Duration::from_millis(1500)),
            flush_failures: 5,
            flush_conflicts: 1,
        }
    }

    /// Catches: a tier's number put on another field of the wire (every value is distinct,
    /// so a swap shows), a node's values changed on the way to the frontend, and the age
    /// not in milliseconds.
    #[test]
    fn a_tier_health_survives_the_wire_field_for_field() {
        let vitals = Vitals::from_tier(Some(health()));
        assert_eq!(
            vitals,
            Vitals {
                unflushed_bytes: Some(300),
                unflushed_blocks: Some(2),
                oldest_unflushed_ms: Some(1500),
                budget_bytes: Some(1000),
                used_bytes: Some(400),
                flush_failures: Some(5),
                flush_conflicts: Some(1),
            }
        );
        let beat = vitals.put_on(pb::HeartbeatRequest::default());
        let received = pb::HeartbeatRequest::decode(&beat.encode_to_vec()[..]).expect("decodes");
        assert_eq!(Vitals::from_wire(&received), vitals);
    }

    /// Catches: no tier (or no block waiting) answered as numbers it does not have: no tier
    /// says nothing at all, and an idle tier says a real 0 age, not unknown.
    #[test]
    fn no_tier_is_unknown_and_an_idle_one_is_zero() {
        assert_eq!(Vitals::from_tier(None), Vitals::NONE);
        assert_eq!(Vitals::NONE, Vitals::default());
        let idle = TierHealth {
            oldest_unflushed: None,
            unflushed_bytes: 0,
            ..health()
        };
        let vitals = Vitals::from_tier(Some(idle));
        assert_eq!(vitals.oldest_unflushed_ms, Some(0));
        assert_eq!(vitals.unflushed_bytes, Some(0));
    }

    /// Catches: a bound at the wrong place. The largest exact JSON integer is kept, one more
    /// is unknown, and each field is bounded on its own (the others stay).
    #[test]
    fn an_absurd_value_is_dropped_to_unknown_and_the_limit_is_kept() {
        let beat = pb::HeartbeatRequest {
            unflushed_bytes: Some(MAX_REPORTED),
            unflushed_blocks: Some(MAX_REPORTED + 1),
            oldest_unflushed_ms: Some(u64::MAX),
            budget_bytes: Some(7),
            ..pb::HeartbeatRequest::default()
        };
        let vitals = Vitals::from_wire(&beat);
        assert_eq!(vitals.unflushed_bytes, Some(MAX_REPORTED));
        assert_eq!(vitals.unflushed_blocks, None);
        assert_eq!(vitals.oldest_unflushed_ms, None);
        assert_eq!(vitals.budget_bytes, Some(7));
        assert_eq!(vitals.used_bytes, None);
        assert_eq!(MAX_REPORTED, 9_007_199_254_740_991);
    }

    /// Catches: the stored row losing a value or turning an unknown into 0 (an unset field
    /// and a stored 0 must stay different), and a field number in the row moved.
    #[test]
    fn the_stored_form_keeps_unknown_apart_from_zero() {
        let vitals = Vitals {
            unflushed_bytes: Some(0),
            flush_failures: Some(9),
            ..Vitals::NONE
        };
        let stored = StoredVitals::from(vitals);
        assert_eq!(stored.encode_to_vec(), [0x08, 0, 0x30, 9], "fields 1 and 6");
        let back = StoredVitals::decode(&stored.encode_to_vec()[..]).expect("decodes");
        assert_eq!(Vitals::from(back), vitals);
        assert!(StoredVitals::from(Vitals::NONE).encode_to_vec().is_empty());
    }
}
