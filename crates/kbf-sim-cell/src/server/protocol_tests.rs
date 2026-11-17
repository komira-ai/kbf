//! The server's protocol handling, driven message by message: how `FindMissingBlobs` is
//! answered and which messages the server leaves alone.

use super::tests::{input, server};
use super::*;

/// What the server sent: every message in its out queue.
fn sent(s: &mut Server) -> Vec<(NodeId, Msg)> {
    s.take_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::Send { to, msg } => Some((to, msg)),
            _ => None,
        })
        .collect()
}

/// A blob the index lacks is answered missing, to the asker, and is not recorded as
/// answered present.
#[test]
fn a_blob_the_index_lacks_is_answered_missing() {
    let mut s = server(Vec::new());
    s.apply(input(0, Event::Start));
    s.take_outputs();
    let digest = crate::input_of(&crate::action(3));
    let msg = Msg::FindMissing {
        digests: vec![digest],
    };
    let from = NodeId::from("client-0");
    s.apply(input(
        5,
        Event::Message {
            from: from.clone(),
            msg,
        },
    ));
    let replies = sent(&mut s);
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].0, from);
    assert_eq!(
        format!("{:?}", replies[0].1),
        format!("Missing {{ asked: [{digest:?}], missing: [{digest:?}] }}")
    );
    assert!(s.seen.present.is_empty());
}

/// A message meant for a client is neither answered nor recorded by the server.
#[test]
fn a_message_for_a_client_is_ignored() {
    let mut s = server(Vec::new());
    s.apply(input(0, Event::Start));
    s.take_outputs();
    let digest = crate::action(3);
    for msg in [
        Msg::Uploaded { digest },
        Msg::UploadFailed { digest },
        Msg::Missing {
            asked: vec![digest],
            missing: vec![digest],
        },
    ] {
        let from = NodeId::from("client-0");
        s.apply(input(5, Event::Message { from, msg }));
        assert!(sent(&mut s).is_empty());
    }
    assert!(s.seen.present.is_empty());
    assert!(s.seen.uploaded.is_empty());
}
