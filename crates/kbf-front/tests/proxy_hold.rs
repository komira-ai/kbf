//! The proxy's raw ByteStream pass-through when no storage node of a blob can answer: the
//! call is held and tried again (`kbf-route`'s pauses and bound) against the placement as
//! it is by then, and a call that cannot be tried again is not held. The nodes are the
//! fakes of `proxy/harness.rs`; the clock is the test's, so nothing sleeps.

#[path = "proxy/harness.rs"]
mod harness;

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream;
use harness::{CHUNK, Fake, Switch, blob, front_over, node, pattern, routed_owners_over, upload};
use kbf_auth::Authorizers;
use kbf_front::Owners;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest, WriteRequest};
use kbf_route::{Hold, HoldClock};
use kbf_types::{Digest, DigestFunction};
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

/// How long the frontends of these tests hold a call.
const BOUND: Duration = Duration::from_secs(60);

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// Something that happens while a hold sleeps (a node comes back, say).
type Action = Box<dyn FnOnce() + Send>;

/// A clock a test drives. Sleeping takes no time: it moves the clock on by the pause, writes
/// the pause down and then does the next of the actions it was given. A hold that runs on
/// past twice the bound is a defect, and stops the test with a panic instead of hanging it.
struct FakeClock {
    elapsed: Mutex<Duration>,
    pauses: Mutex<Vec<Duration>>,
    actions: Mutex<VecDeque<Action>>,
}

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            elapsed: Mutex::default(),
            pauses: Mutex::default(),
            actions: Mutex::default(),
        })
    }

    /// `action` is done at the first sleep that no earlier action was given to.
    fn then(&self, action: Action) {
        self.actions.lock().expect("lock").push_back(action);
    }

    /// The pauses slept, in order.
    fn pauses(&self) -> Vec<Duration> {
        self.pauses.lock().expect("lock").clone()
    }
}

impl HoldClock for FakeClock {
    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().expect("lock")
    }

    fn sleep(&self, pause: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut elapsed = self.elapsed.lock().expect("lock");
            *elapsed += pause;
            assert!(*elapsed <= BOUND * 2, "the hold outran its bound");
            drop(elapsed);
            self.pauses.lock().expect("lock").push(pause);
            let next = self.actions.lock().expect("lock").pop_front();
            next.into_iter().for_each(|action| action());
        })
    }
}

/// The map of `generation` whose three members (1 to 3) are in `states`, writing each blob
/// twice.
fn map_of(generation: u64, states: [MemberState; 3]) -> Arc<Map> {
    let members = (1..=3)
        .zip(states)
        .map(|(id, state)| Member {
            id: MemberId(id),
            addr: format!("127.0.0.1:{id}"),
            zone: String::new(),
            weight: 1,
            state,
            boot: 0,
        })
        .collect();
    Arc::new(Map::new(generation, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("a map"))
}

/// The map of `generation` whose three members are all in `state`.
fn map(generation: u64, state: MemberState) -> Arc<Map> {
    map_of(generation, [state; 3])
}

/// How long a call of these tests may take in real time. The fake clock needs none, so a
/// call that works is done long before.
const LIMIT: Duration = Duration::from_secs(30);

/// `call`, which a defect could make never end: it fails the test after [`LIMIT`].
async fn bounded<T>(call: impl Future<Output = T>) -> T {
    tokio::time::timeout(LIMIT, call).await.expect("hung")
}

/// The digest a ByteStream name of `kind` and `size` names.
fn digest_of(kind: char, size: u64) -> Digest {
    Digest::parse(
        DigestFunction::Sha256,
        &format!("{}/{size}", harness::hash(kind)),
    )
    .expect("a digest")
}

/// Three fake nodes behind a frontend routed by a map the test moves, which holds by a
/// clock the test drives.
struct Farm {
    fakes: Vec<Arc<Fake>>,
    maps: Arc<Switch>,
    clock: Arc<FakeClock>,
    front: harness::Front,
}

/// A farm over `fakes` (member 1 first) and the map `start`, where the members `dead` (by
/// number) are closed ports: nodes that are dead but that the map may still name active.
async fn farm_of(fakes: Vec<Arc<Fake>>, dead: &[usize], start: &Arc<Map>) -> Farm {
    let mut addrs = Vec::new();
    for (i, fake) in fakes.iter().enumerate() {
        addrs.push(if dead.contains(&(i + 1)) {
            ([127, 0, 0, 1], 1).into()
        } else {
            node(fake).await
        });
    }
    let clock = FakeClock::new();
    let maps = Arc::new(Switch(Mutex::new(Arc::clone(start))));
    let hold = Hold::new(BOUND).with_clock(clock.clone());
    let owners = routed_owners_over(maps.clone(), &addrs).with_hold(hold);
    let front = front_over(owners, Authorizers::allow_all(), None).await;
    Farm {
        fakes,
        maps,
        clock,
        front,
    }
}

/// A frontend over the one node `fake` that `--storage` names (no map), which holds by
/// `clock`.
async fn fixed(fake: &Arc<Fake>, clock: &Arc<FakeClock>) -> harness::Front {
    let hold = Hold::new(BOUND).with_clock(clock.clone());
    let only = harness::owner("only", node(fake).await);
    let owners = Owners::ranked(vec![only]).with_hold(hold);
    front_over(owners, Authorizers::allow_all(), None).await
}

/// Three nodes that serve.
fn serving() -> Vec<Arc<Fake>> {
    (0..3).map(|_| Fake::new(false)).collect()
}

/// Three nodes that answer every Read and Write UNAVAILABLE, as if restarting for good.
fn nobody() -> Vec<Arc<Fake>> {
    (0..3).map(|_| Fake::away_for(usize::MAX)).collect()
}

fn calls(fakes: &[Arc<Fake>]) -> Vec<usize> {
    fakes.iter().map(|f| f.calls.load(SeqCst)).collect()
}

/// What the map does at the sleep it is given to: from then on it is `map`.
fn becomes(maps: &Arc<Switch>, map: Arc<Map>) -> Action {
    let maps = Arc::clone(maps);
    Box::new(move || maps.set(map))
}

fn client(front: &harness::Front) -> ByteStreamClient<Channel> {
    ByteStreamClient::new(front.channel.clone())
}

/// A Read of the blob of `kind` and `size`: the bytes that came, and how the stream ended.
async fn read(front: &harness::Front, kind: char, size: u64) -> (usize, Result<(), Status>) {
    let request = ReadRequest {
        resource_name: blob("main", kind, size),
        read_offset: 0,
        read_limit: 0,
    };
    let mut stream = match client(front).read(request).await {
        Ok(response) => response.into_inner(),
        Err(status) => return (0, Err(status)),
    };
    let mut bytes = 0;
    loop {
        match stream.message().await {
            Ok(Some(message)) => bytes += message.data.len(),
            Ok(None) => return (bytes, Ok(())),
            Err(status) => return (bytes, Err(status)),
        }
    }
}

/// An upload of one message small enough to be one HTTP/2 data frame: all of it is the
/// part of the request the proxy keeps to send again.
fn small_message(name: String) -> impl futures::Stream<Item = WriteRequest> {
    stream::iter([WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: true,
        data: vec![pattern(0); 1000],
    }])
}

/// A Write of the blob of `kind` and `size`, in one message.
async fn write(front: &harness::Front, kind: char, size: u64) -> Result<(), Status> {
    let message = small_message(upload("main", kind, size));
    client(front).write(Request::new(message)).await.map(drop)
}

/// Catches a ByteStream Read that fails while every storage node of its blob is away, the
/// defect the hold is for (mutant: no hold), and a hold that reads the placement once
/// (mutant: the owners looked up before the loop): every owner is `Down` in the map, so
/// the first try has nobody to call, and the map moves to a newer generation with the
/// nodes active during the first pause. The second try is served by the first owner of
/// the new map, the only node called, after one pause of 250 ms.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_is_held_until_an_owner_returns() {
    let back = map(2, MemberState::Active);
    let farm = farm_of(serving(), &[], &map(1, MemberState::Down)).await;
    assert_eq!(farm.front.owners.hold().bound(), BOUND);
    farm.clock.then(becomes(&farm.maps, Arc::clone(&back)));

    let (bytes, ended) = read(&farm.front, '0', 7).await;
    ended.expect("served after the hold");
    assert_eq!(bytes, 7);
    assert_eq!(farm.clock.pauses(), [ms(250)]);
    let first = back.read_order(&digest_of('0', 7))[0].0 as usize;
    let mut want = vec![0, 0, 0];
    want[first - 1] = 1;
    assert_eq!(calls(&farm.fakes), want);
}

/// Catches a Read that fails because every owner answers UNAVAILABLE before its first byte,
/// as nodes do while they restart (mutant: no hold): the two owners of the blob each
/// refuse their first call, so the first try gets nothing, and after one pause the first
/// owner serves the whole blob. The third node is never called.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_every_owner_refuses_is_tried_again() {
    let map = map(1, MemberState::Active);
    let fakes: Vec<_> = (0..3).map(|_| Fake::away_for(1)).collect();
    let farm = farm_of(fakes, &[], &map).await;
    let order = map.read_order(&digest_of('0', 7));

    let (bytes, ended) = read(&farm.front, '0', 7).await;
    ended.expect("served after the hold");
    assert_eq!(bytes, 7);
    assert_eq!(farm.clock.pauses(), [ms(250)]);
    let mut want = vec![0, 0, 0];
    want[order[0].0 as usize - 1] = 2;
    want[order[1].0 as usize - 1] = 1;
    assert_eq!(calls(&farm.fakes), want);
}

/// Catches a Read held for a miss that an owner that is away could not confirm (the other
/// side of mutant: hold on any failure): the first owner says it lacks the blob and the
/// second is restarting, so the blob may be on the second and the read waits one pause for
/// it, then is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_miss_that_an_owner_away_could_not_confirm_is_held() {
    let map = map(1, MemberState::Active);
    let order = map.read_order(&digest_of('0', 7));
    let (lacking, away) = (order[0].0 as usize, order[1].0 as usize);
    let mut fakes = serving();
    fakes[lacking - 1] = Fake::forcing('1');
    fakes[away - 1] = Fake::away_for(1);
    let farm = farm_of(fakes, &[], &map).await;

    let (bytes, ended) = read(&farm.front, '0', 7).await;
    ended.expect("served after the hold");
    assert_eq!(bytes, 7);
    assert_eq!(farm.clock.pauses(), [ms(250)]);
    let mut want = vec![0, 0, 0];
    want[lacking - 1] = 2;
    want[away - 1] = 2;
    assert_eq!(calls(&farm.fakes), want);
}

/// Catches a hold that never ends (mutant: unbounded), and one that ends late (a last pause
/// not cut short at the bound): no node ever answers a Read or an upload, the pauses are
/// kbf-route's (250 ms doubling to 5 s, the last what is left of the bound), 60 s in all
/// in 16 pauses, and the call then fails UNAVAILABLE with the first failure of a node, as
/// it does without a hold. The fake clock panics if the hold runs past twice the bound, and
/// each call has a real-time limit, for a hold that stops sleeping and never ends (mutant:
/// the result of the bound's check ignored).
#[tokio::test(flavor = "multi_thread")]
async fn a_call_no_owner_answers_fails_at_the_bound() {
    let map = map(1, MemberState::Active);

    let farm = farm_of(nobody(), &[], &map).await;
    let (bytes, ended) = bounded(read(&farm.front, '0', 7)).await;
    assert_eq!(bytes, 0);
    let refused = ended.expect_err("nobody answers");
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(refused.message(), "restarting");
    assert_eq!(farm.clock.elapsed(), BOUND);
    let pauses = farm.clock.pauses();
    assert_eq!(pauses.len(), 16);
    assert_eq!(pauses[..3], [ms(250), ms(500), ms(1000)]);
    assert_eq!(pauses.last(), Some(&ms(2250)));

    let farm = farm_of(nobody(), &[], &map).await;
    let taken = bounded(write(&farm.front, '0', 7)).await;
    let refused = taken.expect_err("nobody takes it");
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(refused.message(), "restarting");
    assert_eq!(farm.clock.elapsed(), BOUND);
    assert_eq!(farm.clock.pauses().len(), 16);
}

/// Catches a call held when the map has no owner for it and none comes back: with every
/// member `Down` a Read and a Write each wait out the bound (the map is read again at each
/// try, and is the same), call nobody, and are UNAVAILABLE. Mutant: a Read or Write with no
/// owner fails at once, or the lookup of the writer is not held.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_with_every_owner_down_fails_at_the_bound() {
    let down = map(1, MemberState::Down);

    let farm = farm_of(serving(), &[], &down).await;
    let (_, ended) = read(&farm.front, '0', 7).await;
    assert_eq!(ended.expect_err("no owner").code(), Code::Unavailable);
    assert_eq!(farm.clock.elapsed(), BOUND);
    assert_eq!(farm.clock.pauses().len(), 16);
    assert_eq!(calls(&farm.fakes), vec![0, 0, 0]);

    let farm = farm_of(serving(), &[], &down).await;
    let taken = write(&farm.front, '0', 7).await;
    let refused = taken.expect_err("no writer");
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(farm.clock.elapsed(), BOUND);
    assert_eq!(farm.clock.pauses().len(), 16);
    assert_eq!(calls(&farm.fakes), vec![0, 0, 0]);
}

/// Catches a call held when an owner could answer it (mutant: hold on any failure): with
/// the first owner of the blob a closed port the next serves a Read at once, and takes an
/// upload whose writer is a closed port; a blob every owner says it lacks is NOT_FOUND, and
/// a request every owner refuses (INVALID_ARGUMENT) is that, both at once. Nothing sleeps.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_an_owner_answers_is_not_held() {
    let map = map(1, MemberState::Active);
    let order = map.read_order(&digest_of('0', 7));
    let writers = map.write_owners(&digest_of('0', 7));

    let farm = farm_of(serving(), &[order[0].0 as usize], &map).await;
    let (bytes, ended) = read(&farm.front, '0', 7).await;
    ended.expect("served by the next owner");
    assert_eq!(bytes, 7);
    assert!(farm.clock.pauses().is_empty());

    let farm = farm_of(serving(), &[writers[0].0 as usize], &map).await;
    let taken = write(&farm.front, '0', 7).await;
    taken.expect("the other owner takes the upload");
    assert!(farm.clock.pauses().is_empty());

    for (kind, code) in [('1', Code::NotFound), ('4', Code::InvalidArgument)] {
        let fakes: Vec<_> = (0..3).map(|_| Fake::forcing(kind)).collect();
        let farm = farm_of(fakes, &[], &map).await;
        let (bytes, ended) = read(&farm.front, '0', 7).await;
        assert_eq!(bytes, 0);
        assert_eq!(ended.expect_err("no blob").code(), code, "{kind}");
        assert!(farm.clock.pauses().is_empty(), "{kind}");
    }
}

/// Catches a hold applied to a Read that has passed bytes on (mutant: hold mid-stream): the
/// first owner sends two chunks and breaks with UNAVAILABLE, and the client gets its two
/// chunks and the error at once, to resume from `read_offset`; no other node is asked and
/// nothing sleeps.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_past_its_first_frames_is_not_held() {
    let map = map(1, MemberState::Active);
    let fakes: Vec<_> = (0..3)
        .map(|_| Fake::breaking_reads(Code::Unavailable))
        .collect();
    let farm = farm_of(fakes, &[], &map).await;

    let (bytes, ended) = read(&farm.front, '0', 2 * CHUNK as u64).await;
    assert_eq!(bytes, 2 * CHUNK);
    assert_eq!(ended.expect_err("broke").code(), Code::Unavailable);
    assert!(farm.clock.pauses().is_empty());
    assert_eq!(calls(&farm.fakes).iter().sum::<usize>(), 1);
}

/// Catches an upload that fails while its owners restart (mutant: no hold): both write
/// owners refuse their first call with UNAVAILABLE before reading a byte, so the whole
/// upload (still only the frames the proxy keeps) has nowhere to go; after one pause the
/// first owner takes it, with its bytes intact (the fake checks them).
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_is_held_until_an_owner_takes_it() {
    let map = map(1, MemberState::Active);
    let fakes: Vec<_> = (0..3).map(|_| Fake::away_for(1)).collect();
    let farm = farm_of(fakes, &[], &map).await;
    let writers = map.write_owners(&digest_of('0', 7));

    let taken = write(&farm.front, '0', 7).await;
    taken.expect("taken after the hold");
    assert_eq!(farm.clock.pauses(), [ms(250)]);
    let mut want = vec![0, 0, 0];
    want[writers[0].0 as usize - 1] = 2;
    want[writers[1].0 as usize - 1] = 1;
    assert_eq!(calls(&farm.fakes), want);
}

/// Catches an upload held again with the writer it began with (mutant: the owner kept from
/// before the pause, or the first one tried): all three nodes refuse their first call, and
/// during the first pause the map moves to a generation where the first writer is `Down`.
/// The upload goes to the second writer, which has refused once and serves now; the first
/// is not called again.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_held_again_goes_to_the_writer_of_the_map_by_then() {
    let first = map(1, MemberState::Active);
    let writers = first.write_owners(&digest_of('0', 7));
    let mut states = [MemberState::Active; 3];
    states[writers[0].0 as usize - 1] = MemberState::Down;
    let fakes: Vec<_> = (0..3).map(|_| Fake::away_for(1)).collect();
    let farm = farm_of(fakes, &[], &first).await;
    farm.clock.then(becomes(&farm.maps, map_of(2, states)));

    let taken = write(&farm.front, '0', 7).await;
    taken.expect("taken after the hold");
    assert_eq!(farm.clock.pauses(), [ms(250)]);
    let mut want = vec![0, 0, 0];
    want[writers[0].0 as usize - 1] = 1;
    want[writers[1].0 as usize - 1] = 2;
    assert_eq!(calls(&farm.fakes), want);
}

/// Catches an upload told the lookup's status when its map loses every writer while it is
/// held (mutant: the failure of the node dropped for the "no storage node" status, which
/// says nothing of why): the two write owners refuse their first call, and during the first
/// pause every member goes `Down`. The upload waits out the bound against that map and is
/// UNAVAILABLE with the node's own message.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_whose_map_loses_its_writers_is_told_the_nodes_failure() {
    let fakes: Vec<_> = (0..3).map(|_| Fake::away_for(1)).collect();
    let farm = farm_of(fakes, &[], &map(1, MemberState::Active)).await;
    let down = map(2, MemberState::Down);
    farm.clock.then(becomes(&farm.maps, down));

    let taken = write(&farm.front, '0', 7).await;
    let refused = taken.expect_err("no writer is left");
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(refused.message(), "restarting");
    assert_eq!(farm.clock.elapsed(), BOUND);
    assert_eq!(farm.clock.pauses().len(), 16);
    assert_eq!(calls(&farm.fakes).iter().sum::<usize>(), 2);
}

/// Catches an upload held with the owners it found at its first try (mutant: the writer
/// looked up once): every member is `Down` when it begins, so it has no writer to send to,
/// and the nodes are active again in a newer map after the first pause. The upload goes to
/// the writer of the new map, the only node called.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_with_no_writer_waits_for_the_map() {
    let back = map(2, MemberState::Active);
    let farm = farm_of(serving(), &[], &map(1, MemberState::Down)).await;
    farm.clock.then(becomes(&farm.maps, Arc::clone(&back)));

    let taken = write(&farm.front, '0', 7).await;
    taken.expect("taken after the hold");
    assert_eq!(farm.clock.pauses(), [ms(250)]);
    let writer = back.write_owners(&digest_of('0', 7))[0].0 as usize;
    let mut want = vec![0, 0, 0];
    want[writer - 1] = 1;
    assert_eq!(calls(&farm.fakes), want);
}

/// Catches an upload held after its bytes went to a node (mutant: `Upload::again` ignoring
/// that a frame past the first ones was read, so the hold sends half of a blob): the writer
/// reads the first message of a three-message upload and fails UNAVAILABLE. The proxy no
/// longer has what it passed, so the call ends at once with that failure, for the client
/// to start again; nothing sleeps and no other node is called.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_that_was_passed_on_is_not_held() {
    let map = map(1, MemberState::Active);
    let writer = map.write_owners(&digest_of('0', 7))[0].0 as usize;
    let mut fakes = serving();
    fakes[writer - 1] = Fake::failing_writes_after(1);
    let farm = farm_of(fakes, &[], &map).await;

    let message = |i: usize, last: bool| WriteRequest {
        resource_name: if i == 0 {
            upload("main", '0', 7)
        } else {
            String::new()
        },
        write_offset: (i * CHUNK) as i64,
        finish_write: last,
        data: vec![pattern(i); CHUNK],
    };
    let failed = client(&farm.front)
        .write(Request::new(stream::iter([
            message(0, false),
            message(1, false),
            message(2, true),
        ])))
        .await
        .expect_err("the writer failed mid-upload");
    assert_eq!(failed.code(), Code::Unavailable);
    assert!(farm.clock.pauses().is_empty());
    let mut want = vec![0, 0, 0];
    want[writer - 1] = 1;
    assert_eq!(calls(&farm.fakes), want);
}

/// Catches a status query held (it only says where an upload stands, and a client whose
/// query fails starts the upload again, which is held): with every member `Down` the query
/// is UNAVAILABLE at once and nothing sleeps.
#[tokio::test(flavor = "multi_thread")]
async fn a_status_query_is_not_held() {
    let farm = farm_of(serving(), &[], &map(1, MemberState::Down)).await;

    let refused = client(&farm.front)
        .query_write_status(QueryWriteStatusRequest {
            resource_name: upload("main", '0', 7),
        })
        .await
        .expect_err("no writer");
    assert_eq!(refused.code(), Code::Unavailable);
    assert!(farm.clock.pauses().is_empty());
}

/// Catches a hold left out of the frontend that has no map, only the node `--storage`
/// names (mutant: only routed owners hold): the node refuses its first call with
/// UNAVAILABLE, a Read and then an Upload each wait one pause and are served by it.
#[tokio::test(flavor = "multi_thread")]
async fn a_farm_of_one_fixed_node_is_held_too() {
    let fake = Fake::away_for(1);
    let clock = FakeClock::new();
    let front = fixed(&fake, &clock).await;
    let (bytes, ended) = read(&front, '0', 7).await;
    ended.expect("served after the hold");
    assert_eq!(bytes, 7);
    assert_eq!(clock.pauses(), [ms(250)]);
    assert_eq!(fake.calls.load(SeqCst), 2);

    let fake = Fake::away_for(1);
    let clock = FakeClock::new();
    let front = fixed(&fake, &clock).await;
    let taken = write(&front, '0', 7).await;
    taken.expect("taken after the hold");
    assert_eq!(clock.pauses(), [ms(250)]);
    assert_eq!(fake.calls.load(SeqCst), 2);
}
