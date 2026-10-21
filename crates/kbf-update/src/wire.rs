//! The wire messages of `kbf.update.v1` and the values the client works with: the two
//! directions of the translation, and nothing else. A message the client cannot
//! understand becomes a value it refuses (an order with no verb) rather than an error,
//! so the session never fails on one.

use kbf_proto::update as wire;
use kbf_proto::update::update_order::Source as WireOrderSource;
use kbf_proto::update::update_order::Verb as WireVerb;
use kbf_proto::update::update_status::ReleaseSource as WireSource;
use kbf_proto::update::update_status::State as WireState;

use crate::types::{
    Available, Component, Expected, Offer, Order, ReleaseSource, Source, State, Status, Verb,
};

impl From<wire::Component> for Component {
    fn from(c: wire::Component) -> Self {
        Self {
            name: c.name,
            sha256: c.sha256,
            size: c.size,
            attestation: c.attestation,
        }
    }
}

impl From<wire::UpdateOffer> for Offer {
    fn from(o: wire::UpdateOffer) -> Self {
        Self {
            generation: o.generation,
            commit: o.commit,
            platform: o.platform,
            components: o.components.into_iter().map(Component::from).collect(),
            expected: Expected {
                protocol: o.protocol,
                min_protocol: o.min_protocol,
                state_schema: o.state_schema,
                profile: o.profile,
            },
        }
    }
}

impl From<wire::UpdateOrder> for Order {
    fn from(o: wire::UpdateOrder) -> Self {
        let verb = match WireVerb::try_from(o.verb) {
            Ok(WireVerb::Activate) => Some(Verb::Activate),
            Ok(WireVerb::Confirm) => Some(Verb::Confirm),
            Ok(WireVerb::Revert) => Some(Verb::Revert),
            Ok(WireVerb::Unspecified) | Err(_) => None,
        };
        // The deliberate exception to the verb rule above: zero is a fetch, what every sender
        // that predates the field means. A number this build does not know is refused by the
        // client.
        let source = match WireOrderSource::try_from(o.source) {
            Ok(WireOrderSource::Unspecified | WireOrderSource::Fetch) => Some(Source::Fetch),
            Ok(WireOrderSource::Staged) => Some(Source::Staged),
            Err(_) => None,
        };
        Self {
            order_id: o.order_id,
            epoch: o.epoch,
            generation: o.generation,
            verb,
            commit: o.commit,
            window_s: o.window_s,
            source,
        }
    }
}

impl From<State> for WireState {
    fn from(state: State) -> Self {
        match state {
            State::Idle => Self::Idle,
            State::Fetching => Self::Fetching,
            State::Staged => Self::Staged,
            State::ActivatedUnconfirmed => Self::ActivatedUnconfirmed,
            State::Confirmed => Self::Confirmed,
            State::Reverted => Self::Reverted,
            State::Refused => Self::Refused,
            State::Manual => Self::Manual,
        }
    }
}

impl From<&Available> for wire::update_status::Available {
    fn from(a: &Available) -> Self {
        Self {
            commit: a.commit.clone(),
            seq: a.seq,
        }
    }
}

impl From<ReleaseSource> for WireSource {
    fn from(source: ReleaseSource) -> Self {
        match source {
            ReleaseSource::Unreported => Self::Unspecified,
            ReleaseSource::Url => Self::Url,
            ReleaseSource::Frontend => Self::Frontend,
            ReleaseSource::Bundled => Self::Bundled,
        }
    }
}

impl From<&Status> for wire::UpdateStatus {
    fn from(s: &Status) -> Self {
        let staged = s.staged.unwrap_or_default();
        Self {
            running_commit: s.running_commit.clone(),
            installed_commit: s.installed_commit.clone().unwrap_or_default(),
            staged_commit: s.staged_commit.clone().unwrap_or_default(),
            state: WireState::from(s.state).into(),
            refusal: s.refusal.clone(),
            acked_order_id: s.acked_order_id,
            offered_generation: s.offered_generation,
            helper_present: s.helper_present,
            staged_protocol: staged.protocol,
            staged_min_protocol: staged.min_protocol,
            staged_state_schema: staged.state_schema,
            available: s.available.as_ref().map(Into::into),
            release_source: WireSource::from(s.release_source).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Available, ReleaseSource, Source, Staged};

    /// Catches a field of an offer lost or moved on the way in (every field is given a
    /// different value, so a swap shows).
    #[test]
    fn an_offer_keeps_every_field() {
        let message = wire::UpdateOffer {
            generation: 9,
            commit: "c".repeat(40),
            platform: "darwin-arm64".to_owned(),
            components: vec![wire::Component {
                name: "kbf-daemon".to_owned(),
                sha256: "d".repeat(64),
                size: 77,
                attestation: b"bundle".to_vec(),
            }],
            protocol: 5,
            min_protocol: 4,
            state_schema: 3,
            profile: "release".to_owned(),
        };
        let offer = Offer::from(message);
        assert_eq!(offer.generation, 9);
        assert_eq!(offer.commit, "c".repeat(40));
        assert_eq!(offer.platform, "darwin-arm64");
        assert_eq!(
            offer.components,
            vec![Component {
                name: "kbf-daemon".to_owned(),
                sha256: "d".repeat(64),
                size: 77,
                attestation: b"bundle".to_vec(),
            }]
        );
        assert_eq!(
            offer.expected,
            Expected {
                protocol: 5,
                min_protocol: 4,
                state_schema: 3,
                profile: "release".to_owned()
            }
        );
    }

    /// Catches a verb mapped to another, and an unspecified or unknown verb taken for one:
    /// those become an order with no verb, which the client refuses.
    #[test]
    fn an_order_maps_its_verb() {
        let order = |verb: i32| {
            Order::from(wire::UpdateOrder {
                order_id: 4,
                epoch: 5,
                generation: 6,
                verb,
                commit: "e".repeat(40),
                window_s: 600,
                ..Default::default()
            })
        };
        assert_eq!(order(1).verb, Some(Verb::Activate));
        assert_eq!(order(2).verb, Some(Verb::Confirm));
        assert_eq!(order(3).verb, Some(Verb::Revert));
        assert_eq!(order(0).verb, None);
        assert_eq!(order(99).verb, None);
        let o = order(1);
        assert_eq!((o.order_id, o.epoch, o.window_s), (4, 5, 600));
        assert_eq!(o.generation, 6, "the generation is carried to the record");
        assert_eq!(o.commit, "e".repeat(40));
    }

    /// Catches a source mapped to another, an unset source taken for a refusal (what every
    /// sender that predates the field writes is a fetch), and a number this build does not
    /// know taken for one it does.
    #[test]
    fn an_order_maps_its_source() {
        let source = |n: i32| {
            let message = wire::UpdateOrder {
                source: n,
                ..Default::default()
            };
            Order::from(message).source
        };
        assert_eq!(source(0), Some(Source::Fetch));
        assert_eq!(source(1), Some(Source::Fetch));
        assert_eq!(source(2), Some(Source::Staged));
        assert_eq!(source(3), None);
        assert_eq!(source(99), None);
        assert_eq!(source(-1), None);
    }

    /// Catches a state sent as another, a missing value sent as something but empty or 0,
    /// and a staged release's facts lost.
    #[test]
    fn a_status_maps_to_the_message() {
        let table = [
            (State::Idle, WireState::Idle),
            (State::Fetching, WireState::Fetching),
            (State::Staged, WireState::Staged),
            (State::ActivatedUnconfirmed, WireState::ActivatedUnconfirmed),
            (State::Confirmed, WireState::Confirmed),
            (State::Reverted, WireState::Reverted),
            (State::Refused, WireState::Refused),
            (State::Manual, WireState::Manual),
        ];
        for (state, expected) in table {
            let status = Status {
                state,
                ..Status::manual("0123456789ab")
            };
            assert_eq!(wire::UpdateStatus::from(&status).state, expected as i32);
        }
        let bare = wire::UpdateStatus::from(&Status::manual("0123456789ab"));
        assert_eq!(bare.running_commit, "0123456789ab");
        assert_eq!(bare.installed_commit, "");
        assert_eq!(bare.staged_commit, "");
        assert_eq!(bare.state, WireState::Manual as i32);
        assert!(!bare.helper_present);
        assert_eq!(
            (
                bare.staged_protocol,
                bare.staged_min_protocol,
                bare.staged_state_schema
            ),
            (0, 0, 0)
        );

        let full = Status {
            running_commit: "0123456789ab".to_owned(),
            installed_commit: Some("1".repeat(40)),
            staged_commit: Some("2".repeat(40)),
            state: State::Refused,
            refusal: "why".to_owned(),
            acked_order_id: 6,
            offered_generation: 7,
            helper_present: true,
            staged: Some(Staged {
                protocol: 3,
                min_protocol: 2,
                state_schema: 5,
            }),
            available: Some(Available {
                commit: "3".repeat(40),
                seq: 812,
            }),
            release_source: ReleaseSource::Bundled,
            gated: true,
        };
        let message = wire::UpdateStatus::from(&full);
        let seed = message.available.clone().expect("a seed is reported");
        assert_eq!((seed.commit, seed.seq), ("3".repeat(40), 812));
        assert_eq!(
            message.release_source,
            wire::update_status::ReleaseSource::Bundled as i32
        );
        assert_eq!(bare.available, None, "no seed, nothing reported");
        assert_eq!(bare.release_source, 0, "no source, nothing reported");
        assert_eq!(message.installed_commit, "1".repeat(40));
        assert_eq!(message.staged_commit, "2".repeat(40));
        assert_eq!(message.refusal, "why");
        assert_eq!(message.acked_order_id, 6);
        assert_eq!(message.offered_generation, 7);
        assert!(message.helper_present);
        assert_eq!(
            (
                message.staged_protocol,
                message.staged_min_protocol,
                message.staged_state_schema
            ),
            (3, 2, 5)
        );
    }

    /// Catches a source sent as another, and a source the installer did not report sent as
    /// one it did.
    #[test]
    fn every_release_source_maps_to_its_own_wire_value() {
        use wire::update_status::ReleaseSource as Wire;
        let table = [
            (ReleaseSource::Unreported, Wire::Unspecified),
            (ReleaseSource::Url, Wire::Url),
            (ReleaseSource::Frontend, Wire::Frontend),
            (ReleaseSource::Bundled, Wire::Bundled),
        ];
        for (source, expected) in table {
            let status = Status {
                release_source: source,
                ..Status::manual("0123456789ab")
            };
            let message = wire::UpdateStatus::from(&status);
            assert_eq!(message.release_source, expected as i32, "{source:?}");
        }
    }
}
