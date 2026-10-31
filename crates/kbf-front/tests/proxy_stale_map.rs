//! The proxy's raw ByteStream Write when a node answers `STALE_MAP`: the map is fetched and
//! an upload that is still held whole is sent again to the owner the newer map names, by the
//! router's rule (a fetch that brings nothing newer, or `MAX_MAP_REFETCHES` of them, ends it
//! UNAVAILABLE). An upload already streamed past its first frames cannot be sent again, and
//! no other answer is tried again. The nodes are the fakes of `proxy/harness.rs`.

#[path = "proxy/harness.rs"]
mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use futures::stream;
use harness::{Advancing, CHUNK, Fake, front_over, node, pattern, routed_owners_over, upload};
use kbf_auth::Authorizers;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_route::MAX_MAP_REFETCHES;
use kbf_types::{Digest, DigestFunction};
use tonic::{Code, Request};

/// The map of `generation` whose members have these states.
fn map(generation: u64, states: [MemberState; 3]) -> Arc<Map> {
    let members = states
        .iter()
        .zip(1..)
        .map(|(state, id)| Member {
            id: MemberId(id),
            addr: format!("127.0.0.1:{id}"),
            zone: String::new(),
            weight: 1,
            state: *state,
            boot: 0,
        })
        .collect();
    Arc::new(Map::new(generation, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("a map"))
}

fn digest_of(kind: char, size: u64) -> Digest {
    Digest::parse(
        DigestFunction::Sha256,
        &format!("{}/{size}", harness::hash(kind)),
    )
    .expect("a digest")
}

/// Three nodes behind a frontend that routes by `maps`, and the map source.
struct Rig {
    fakes: Vec<Arc<Fake>>,
    maps: Arc<Advancing>,
    front: harness::Front,
}

async fn rig(fakes: Vec<Arc<Fake>>, maps: Vec<Arc<Map>>) -> Rig {
    let mut addrs = Vec::new();
    for fake in &fakes {
        addrs.push(node(fake).await);
    }
    let maps = Advancing::new(maps);
    let owners = routed_owners_over(maps.clone(), &addrs);
    let front = front_over(owners, Authorizers::allow_all(), None).await;
    Rig { fakes, maps, front }
}

fn calls(fakes: &[Arc<Fake>]) -> Vec<usize> {
    fakes.iter().map(|f| f.calls.load(SeqCst)).collect()
}

/// An upload of one message small enough to be one HTTP/2 data frame: all of it is the part
/// of the request the proxy keeps to send again.
fn small(name: String) -> impl futures::Stream<Item = WriteRequest> {
    stream::iter([WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: true,
        data: vec![pattern(0); 1000],
    }])
}

/// An upload of three messages of 64 KiB, of which a node that has read two has taken bytes
/// past the first frames, which the proxy does not keep.
fn long(name: String) -> impl futures::Stream<Item = WriteRequest> {
    stream::iter((0..3).map(move |i| WriteRequest {
        resource_name: if i == 0 { name.clone() } else { String::new() },
        write_offset: (i * CHUNK / 16) as i64,
        finish_write: i == 2,
        data: vec![pattern(i); CHUNK / 16],
    }))
}

fn client(front: &harness::Front) -> ByteStreamClient<tonic::transport::Channel> {
    ByteStreamClient::new(front.channel.clone())
}

/// A blob, and the first node that the map `first` makes its writer.
fn writer_of(first: &Map) -> (char, u64, usize) {
    let owners = first.write_owners(&digest_of('6', 10));
    ('6', 10, owners[0].0 as usize)
}

/// Catches a STALE_MAP that goes to the client: the first owner answers it once, naming
/// generation 8, and the map fetched names it down; the upload, held whole, is sent again to
/// the owner that map names, tagged with generation 8, and succeeds. The first owner is
/// asked once.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_upload_is_sent_to_the_owner_of_the_newer_map() {
    let all = [MemberState::Active; 3];
    let before = map(7, all);
    let (kind, size, first) = writer_of(&before);
    let mut states = all;
    states[first - 1] = MemberState::Down;
    let after = map(8, states);
    let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    fakes[first - 1] = Fake::stale_for(1, 8, 0);
    let t = rig(fakes, vec![before, after.clone()]).await;

    client(&t.front)
        .write(Request::new(small(upload("main", kind, size))))
        .await
        .expect("the newer map's owner takes the upload");

    let second = after.write_owners(&digest_of(kind, size))[0].0 as usize;
    assert_ne!(second, first);
    let mut want = vec![0, 0, 0];
    want[first - 1] = 1;
    want[second - 1] = 1;
    assert_eq!(calls(&t.fakes), want);
    let seen = t.fakes[second - 1].seen.lock().expect("lock").clone();
    assert_eq!(
        seen[0]
            .metadata
            .get("kbf-map-generation")
            .map(|v| v.to_str().expect("text")),
        Some("8")
    );
    assert_eq!(t.maps.fetches.load(SeqCst), 1);
}

/// Catches an upload sent again without its bytes: a node that has read two of the three
/// messages answers STALE_MAP, the proxy holds only the first frames, so the upload is
/// UNAVAILABLE (the client starts it again), no map is fetched and no other node is asked.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_past_its_first_frames_is_unavailable() {
    let before = map(7, [MemberState::Active; 3]);
    let (kind, size, first) = writer_of(&before);
    let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    fakes[first - 1] = Fake::stale_for(1, 8, 2);
    let t = rig(fakes, vec![before, map(8, [MemberState::Active; 3])]).await;

    let status = client(&t.front)
        .write(Request::new(long(upload("main", kind, size))))
        .await
        .expect_err("cannot be sent again");
    assert_eq!(status.code(), Code::Unavailable, "{status}");
    let mut want = vec![0, 0, 0];
    want[first - 1] = 1;
    assert_eq!(calls(&t.fakes), want);
    assert_eq!(t.maps.fetches.load(SeqCst), 0);
}

/// Catches a loop on a map that never settles: every node answers STALE_MAP and every fetch
/// brings a newer map, so the upload is sent `MAX_MAP_REFETCHES` times again and then ends
/// UNAVAILABLE, after that many fetches and no more.
#[tokio::test(flavor = "multi_thread")]
async fn a_map_that_never_settles_stops_at_the_routers_bound() {
    let maps: Vec<_> = (7..40).map(|g| map(g, [MemberState::Active; 3])).collect();
    let (kind, size, _) = writer_of(&maps[0]);
    let fakes: Vec<_> = (0..3).map(|_| Fake::stale_for(usize::MAX, 99, 0)).collect();
    let t = rig(fakes, maps).await;

    let status = client(&t.front)
        .write(Request::new(small(upload("main", kind, size))))
        .await
        .expect_err("the map never settles");
    assert_eq!(status.code(), Code::Unavailable, "{status}");
    assert_eq!(t.maps.fetches.load(SeqCst), MAX_MAP_REFETCHES);
    assert_eq!(calls(&t.fakes).iter().sum::<usize>(), 1 + MAX_MAP_REFETCHES);
}

/// Catches a retry of what is not STALE_MAP: the writer refuses the upload with
/// INVALID_ARGUMENT, or with FAILED_PRECONDITION that names no map generation (a refused
/// action-cache write has the same code); each is the client's answer from one call, with
/// no fetch.
#[tokio::test(flavor = "multi_thread")]
async fn another_answer_is_not_tried_again() {
    for code in [Code::InvalidArgument, Code::FailedPrecondition] {
        let before = map(7, [MemberState::Active; 3]);
        let (kind, size, first) = writer_of(&before);
        let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
        fakes[first - 1] = Fake::refusing_writes(code);
        let t = rig(fakes, vec![before, map(8, [MemberState::Active; 3])]).await;

        let status = client(&t.front)
            .write(Request::new(small(upload("main", kind, size))))
            .await
            .expect_err("refused");
        assert_eq!(status.code(), code);
        let mut want = vec![0, 0, 0];
        want[first - 1] = 1;
        assert_eq!(calls(&t.fakes), want);
        assert_eq!(t.maps.fetches.load(SeqCst), 0);
    }
}
