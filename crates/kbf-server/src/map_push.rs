//! The storage map on a daemon's session (`docs/design/architecture-v2.md`, section 6):
//! the map in `Welcome`, a `MapUpdate` each time the map changes, and the answer to a
//! `MapRequest`. The map comes from a [`MapSource`]; a server given none (a farm of one
//! member) sends no map and does not answer a request.

use std::sync::Arc;

use kbf_mapsync::MapSource;
use kbf_proto::kbf::map::v1 as wire;
use kbf_proto::worker::{MapUpdate, server_message};
use tokio::sync::watch;

use crate::farm::Outbound;

/// The storage map a worker service sends, if it has one: the code the generic session
/// handler calls, so that none of it is conditional there (generic code is covered once per
/// instantiation of the farm's log and store types, and this is not generic).
#[derive(Clone)]
pub(crate) struct MapFeed(Option<Arc<dyn MapSource>>);

impl std::fmt::Debug for MapFeed {
    /// The source is named, not printed: implementors hold state stores and channels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "MapFeed(MapSource)"
        } else {
            "MapFeed(none)"
        })
    }
}

impl MapFeed {
    /// No map: a farm of one member.
    pub(crate) const NONE: Self = Self(None);

    pub(crate) fn new(source: Arc<dyn MapSource>) -> Self {
        Self(Some(source))
    }

    /// A receiver of the source's changes. Taken before [`Self::welcome_map`], so a change
    /// between the two is sent again as a `MapUpdate`, never missed.
    pub(crate) fn subscribe(&self) -> Option<watch::Receiver<u64>> {
        self.0.as_ref().map(|source| source.changes())
    }

    /// The map the source holds now, as the wire has it, for `Welcome`.
    pub(crate) fn welcome_map(&self) -> Option<wire::Map> {
        self.0
            .as_ref()
            .map(|source| wire::Map::from(&*source.current()))
    }

    /// Starts sending a `MapUpdate` on `outbound` for each change in `changes` (from
    /// [`Self::subscribe`]), until the stream is gone. Called after the `Welcome` was
    /// queued on `outbound`, so the first update follows it.
    pub(crate) fn push(&self, changes: Option<watch::Receiver<u64>>, outbound: &Outbound) {
        if let Some((source, changes)) = self.0.clone().zip(changes) {
            tokio::spawn(push_changes(source, changes, outbound.clone()));
        }
    }

    /// The answer to a `MapRequest`: a `MapUpdate` of the map held now, when there is a
    /// source.
    pub(crate) fn answer(&self) -> Option<server_message::Message> {
        self.0.as_deref().map(update)
    }
}

/// A `MapUpdate` of the map `source` holds now.
fn update(source: &dyn MapSource) -> server_message::Message {
    server_message::Message::MapUpdate(MapUpdate {
        map: Some(wire::Map::from(&*source.current())),
    })
}

/// Sends a `MapUpdate` on `outbound` each time `changes` ticks, until the stream is gone
/// or the source is.
///
/// Latest wins: a tick is sent as the map the source holds when the task gets to it, and
/// ticks that came while it was busy are one. `changes` is a receiver taken before the
/// `Welcome` was built, so a map that changed meanwhile is sent again (the receiver
/// ignores a generation it holds), never missed.
pub(crate) async fn push_changes(
    source: Arc<dyn MapSource>,
    mut changes: watch::Receiver<u64>,
    outbound: Outbound,
) {
    let sending = async {
        while changes.changed().await.is_ok() {
            // A send fails only for a stream that is gone, which `closed` below ends.
            let _ = outbound.send(Ok(crate::worker::message(update(&*source))));
        }
    };
    tokio::select! {
        () = outbound.closed() => {}
        () = sending => {}
    }
}

#[cfg(test)]
mod tests {
    use kbf_map::{Map, Member, MemberId, MemberState, Rule};
    use kbf_mapsync::FakeMapSource;
    use kbf_proto::worker::ServerMessage;
    use tokio::sync::mpsc;

    use super::*;

    fn map(generation: u64) -> Map {
        let member = Member {
            id: MemberId(1),
            addr: "127.0.0.1:1".to_owned(),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
        };
        Map::new(generation, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map")
    }

    /// The message a `MapUpdate` of `generation` is.
    fn update_of(generation: u64) -> ServerMessage {
        ServerMessage {
            message: Some(server_message::Message::MapUpdate(MapUpdate {
                map: Some(wire::Map::from(&map(generation))),
            })),
        }
    }

    /// Catches a `Debug` of the service that prints the map source's state (a state store
    /// handle, a channel), or fails to build when a source has no `Debug`.
    #[test]
    fn a_map_source_is_named_not_printed() {
        let source = Arc::new(FakeMapSource::new(map(1)));
        assert_eq!(format!("{:?}", MapFeed::new(source)), "MapFeed(MapSource)");
        assert_eq!(format!("{:?}", MapFeed::NONE), "MapFeed(none)");
    }

    /// Catches a pusher that sends the map it was subscribed at instead of the one the
    /// source holds when it wakes, and one that sends each tick of a burst as a map of
    /// its own (the receiver would take them in turn, and a slow one would fall behind).
    #[tokio::test(start_paused = true)]
    async fn the_pusher_sends_the_latest_map() {
        let source = Arc::new(FakeMapSource::new(map(1)));
        let (outbound, mut sent) = mpsc::unbounded_channel();
        let pusher = tokio::spawn(push_changes(
            Arc::clone(&source) as Arc<dyn MapSource>,
            source.changes(),
            outbound,
        ));
        // Two changes before the pusher runs: one update, of the later map.
        source.set(map(2));
        source.set(map(3));
        assert_eq!(
            sent.recv().await.expect("an update").expect("not an error"),
            update_of(3)
        );
        source.set(map(4));
        assert_eq!(
            sent.recv().await.expect("an update").expect("not an error"),
            update_of(4)
        );
        pusher.abort();
    }

    /// Catches a pusher that outlives its stream: it holds the source until the source
    /// changes, which on a quiet farm is never.
    #[tokio::test(start_paused = true)]
    async fn the_pusher_stops_when_the_stream_is_gone() {
        let source = Arc::new(FakeMapSource::new(map(1)));
        let (outbound, sent) = mpsc::unbounded_channel();
        let pusher = tokio::spawn(push_changes(
            Arc::clone(&source) as Arc<dyn MapSource>,
            source.changes(),
            outbound,
        ));
        tokio::task::yield_now().await;
        drop(sent);
        pusher.await.expect("the pusher ends");
    }

    /// Catches a pusher that waits for ever on a source whose changes can no longer
    /// come (a receiver whose sender is gone).
    #[tokio::test(start_paused = true)]
    async fn the_pusher_stops_when_the_source_cannot_change() {
        let source = Arc::new(FakeMapSource::new(map(1)));
        let (dead, changes) = watch::channel(1);
        drop(dead);
        let (outbound, _sent) = mpsc::unbounded_channel();
        push_changes(source, changes, outbound).await;
    }
}
