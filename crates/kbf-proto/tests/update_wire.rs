//! Byte-for-byte goldens for `kbf.update.v1` and its three places in `kbf.worker.v1`.
//!
//! The expected bytes were written out by hand from the proto numbers (field number
//! `n`, wire type `w`: tag `n << 3 | w`, then a varint or a length and the bytes), not
//! taken from prost's output, so they pin the wire contract that the node side and the
//! frontend build against independently. Each constant is a list of the fields in order
//! with the field named in the comment above it.
//!
//! Catches:
//! - a renumbered or retyped field of any message (the bytes change);
//! - a field of the new messages that prost would encode in another order than by
//!   number, or a default that is written (proto3 writes none);
//! - `update_status`, `update_offer` or `update_order` moved off field 7 of
//!   `DaemonMessage`, 8 and 9 of `ServerMessage`;
//! - an enum value renumbered, or a service renamed (the path a client dials).

use kbf_proto::update::update_order::{Source as OrderSource, Verb};
use kbf_proto::update::update_status::{Available, ReleaseSource, State};
use kbf_proto::update::{
    Component, DescribeRequest, FetchChunk, FetchRequest, ReleaseComponent, ReleaseDescription,
    UpdateOffer, UpdateOrder, UpdateStatus, release_service_server::ReleaseServiceServer,
};
use kbf_proto::worker::{DaemonMessage, ServerMessage, daemon_message, server_message};
use prost::Message;
use tonic::server::NamedService;

const SHA: &str = "abababababababababababababababababababababababababababababababab";
const OTHER_SHA: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

const COMPONENT: &[&str] = &[
    // name = 'kbf'
    "0a036b6266",
    // sha256
    "124061626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162",
    // size = 1234
    "18d209",
    // attestation
    "22087b2261223a317d0a",
];

const OFFER: &[&str] = &[
    // generation = 7
    "0807",
    // commit
    "122830313233343536373839616263646566303132333435363738396162636465663031323334353637",
    // platform = linux-x86_64
    "1a0c6c696e75782d7838365f3634",
    // components[0]: tag 4, length 0x54
    "22540a036b626612406162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616218d20922087b2261223a317d0a",
    // components[1] (kbf-daemon)
    "22530a0a6b62662d6461656d6f6e1240636463646364636463646364636463646364636463646364636463646364636463646364636463646364636463646364636463646364636463646364636463641805220178",
    // protocol = 2, min_protocol = 1, state_schema = 4, profile = release
    "2802",
    "3001",
    "3804",
    "420772656c65617365",
];

const ORDER: &[&str] = &[
    // order_id = 3
    "0803",
    // epoch = 9
    "1009",
    // verb = VERB_ACTIVATE
    "1801",
    // commit
    "222830313233343536373839616263646566303132333435363738396162636465663031323334353637",
    // window_s = 900
    "288407",
    // generation = 11
    "300b",
];

const STATUS: &[&str] = &[
    // running_commit
    "0a0c303132333435363738396162",
    // installed_commit
    "122830313233343536373839616263646566303132333435363738396162636465663031323334353637",
    // staged_commit
    "1a2866666666666666666666666666666666666666666666666666666666666666666666666666666666",
    // state = ACTIVATED_UNCONFIRMED
    "2004",
    // refusal
    "2a026e6f",
    // acked_order_id = 3
    "3003",
    // offered_generation = 7
    "3807",
    // helper_present = true
    "4001",
    // staged_protocol = 2
    "4802",
    // staged_min_protocol = 1
    "5001",
    // staged_state_schema = 4
    "5804",
];

/// The two fields a host adds when its package carries a seed, in the order they follow the
/// others: `available`, then `release_source`.
const PACKAGE: &[&str] = &[
    // available: tag 12, length 0x2d = 45
    "622d",
    // available.commit
    "0a2830313233343536373839616263646566303132333435363738396162636465663031323334353637",
    // available.seq = 812
    "10ac06",
    // release_source = RELEASE_SOURCE_BUNDLED
    "6803",
];

const FETCH_REQUEST: &[&str] = &[
    // sha256
    "0a4061626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162",
];

const FETCH_CHUNK: &[&str] = &[
    // data = 00 01 ff
    "0a030001ff",
];

const DESCRIBE_REQUEST: &[&str] = &[
    // commit
    "0a2830313233343536373839616263646566303132333435363738396162636465663031323334353637",
    // platform = linux-x86_64
    "120c6c696e75782d7838365f3634",
];

const RELEASE_COMPONENT: &[&str] = &[
    // name = 'kbf'
    "0a036b6266",
    // platform = linux-x86_64
    "120c6c696e75782d7838365f3634",
    // tarball = kbf-<commit>-linux-x86_64.tar.gz: tag 3, length 0x40 = 64
    "1a40",
    "6b62662d",
    "30313233343536373839616263646566303132333435363738396162636465663031323334353637",
    "2d",
    "6c696e75782d7838365f3634",
    "2e7461722e677a",
    // sha256
    "224061626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162",
    // size = 1234
    "28d209",
    // bundle_sha256
    "324063646364636463646364636463646364636463646364636463646364636463646364636463646364636463646364636463646364636463646364636463646364",
    // bundle_size = 5000
    "388827",
];

const RELEASE_DESCRIPTION_COMMIT: &[&str] = &[
    // commit
    "0a2830313233343536373839616263646566303132333435363738396162636465663031323334353637",
];

/// The bytes the hex pieces spell, in order.
fn bytes(pieces: &[&str]) -> Vec<u8> {
    hex::decode(pieces.concat()).expect("the golden is hex")
}

/// `prefix` (a hex tag and length) and then `body`.
fn framed(prefix: &str, body: &[&str]) -> Vec<u8> {
    let mut out = hex::decode(prefix).expect("hex");
    out.extend(bytes(body));
    out
}

fn kbf() -> Component {
    Component {
        name: "kbf".to_owned(),
        sha256: SHA.to_owned(),
        size: 1234,
        attestation: b"{\"a\":1}\n".to_vec(),
    }
}

fn kbf_daemon() -> Component {
    Component {
        name: "kbf-daemon".to_owned(),
        sha256: OTHER_SHA.to_owned(),
        size: 5,
        attestation: b"x".to_vec(),
    }
}

fn offer() -> UpdateOffer {
    UpdateOffer {
        generation: 7,
        commit: COMMIT.to_owned(),
        platform: "linux-x86_64".to_owned(),
        components: vec![kbf(), kbf_daemon()],
        protocol: 2,
        min_protocol: 1,
        state_schema: 4,
        profile: "release".to_owned(),
    }
}

fn order() -> UpdateOrder {
    UpdateOrder {
        order_id: 3,
        epoch: 9,
        verb: Verb::Activate.into(),
        commit: COMMIT.to_owned(),
        window_s: 900,
        generation: 11,
        source: 0,
    }
}

fn status() -> UpdateStatus {
    UpdateStatus {
        running_commit: "0123456789ab".to_owned(),
        installed_commit: COMMIT.to_owned(),
        staged_commit: "f".repeat(40),
        state: State::ActivatedUnconfirmed.into(),
        refusal: "no".to_owned(),
        acked_order_id: 3,
        offered_generation: 7,
        helper_present: true,
        staged_protocol: 2,
        staged_min_protocol: 1,
        staged_state_schema: 4,
        available: None,
        release_source: ReleaseSource::Unspecified.into(),
    }
}

/// Encodes `message` and checks it is `golden`, and that `golden` decodes to `message`.
fn golden<M: Message + Default + PartialEq + std::fmt::Debug>(message: &M, golden: &[u8]) {
    assert_eq!(message.encode_to_vec(), golden);
    assert_eq!(&M::decode(golden).expect("decodes"), message);
}

#[test]
fn component_bytes() {
    golden(&kbf(), &bytes(COMPONENT));
}

#[test]
fn offer_bytes() {
    golden(&offer(), &bytes(OFFER));
}

#[test]
fn order_bytes() {
    golden(&order(), &bytes(ORDER));
}

/// Catches `source` renumbered off field 7 (tag `0x38`), `FETCH` and `STAGED` swapped, and a
/// number the node does not know refused at decode instead of by the node (it must survive as
/// the number it is). An unset source writing nothing is pinned by `order_bytes`.
#[test]
fn a_staged_order_is_field_7_and_an_explicit_fetch_is_1() {
    let staged = UpdateOrder {
        source: OrderSource::Staged.into(),
        ..order()
    };
    golden(&staged, &bytes(&[ORDER, &["3802"][..]].concat()));
    let fetch = UpdateOrder {
        source: OrderSource::Fetch.into(),
        ..UpdateOrder::default()
    };
    golden(&fetch, &bytes(&["3801"]));
    let unknown = UpdateOrder::decode(&bytes(&["3809"])[..]).expect("decodes");
    assert_eq!(unknown.source, 9);
}

#[test]
fn status_bytes() {
    golden(&status(), &bytes(STATUS));
}

/// Catches a renumbered or retyped `available` (field 12, a message of the commit and a
/// two-byte varint `seq`) or `release_source` (field 13, an enum), the enum's `bundled`
/// renumbered, and either written before a field of a lower number. A host that reports
/// neither writes neither (`defaults_are_not_written`), so an older frontend reads the rest.
#[test]
fn status_reports_the_package_and_the_source() {
    let package = UpdateStatus {
        available: Some(Available {
            commit: COMMIT.to_owned(),
            seq: 812,
        }),
        release_source: ReleaseSource::Bundled.into(),
        ..UpdateStatus::default()
    };
    golden(&package, &bytes(PACKAGE));
    let full = UpdateStatus {
        available: package.available.clone(),
        release_source: package.release_source,
        ..status()
    };
    let mut want = bytes(STATUS);
    want.extend(bytes(PACKAGE));
    golden(&full, &want);
    // The other sources are the numbers the enum says.
    let numbers = [
        (ReleaseSource::Unspecified, 0),
        (ReleaseSource::Url, 1),
        (ReleaseSource::Frontend, 2),
        (ReleaseSource::Bundled, 3),
    ];
    for (source, number) in numbers {
        assert_eq!(source as i32, number);
    }
}

#[test]
fn fetch_bytes() {
    let request = FetchRequest {
        sha256: SHA.to_owned(),
    };
    golden(&request, &bytes(FETCH_REQUEST));
    let chunk = FetchChunk {
        data: vec![0, 1, 0xff],
    };
    golden(&chunk, &bytes(FETCH_CHUNK));
}

fn release_component() -> ReleaseComponent {
    ReleaseComponent {
        name: "kbf".to_owned(),
        platform: "linux-x86_64".to_owned(),
        tarball: format!("kbf-{COMMIT}-linux-x86_64.tar.gz"),
        sha256: SHA.to_owned(),
        size: 1234,
        bundle_sha256: OTHER_SHA.to_owned(),
        bundle_size: 5000,
    }
}

/// Catches a renumbered or retyped field of `Describe`'s request, the component it
/// describes or the description (the two-byte varints of 1234 and 5000 and the two-byte
/// length of the nested component pin the encoding too), and a component that is not field
/// 2 of the description.
#[test]
fn describe_bytes() {
    let request = DescribeRequest {
        commit: COMMIT.to_owned(),
        platform: "linux-x86_64".to_owned(),
    };
    golden(&request, &bytes(DESCRIBE_REQUEST));
    golden(&release_component(), &bytes(RELEASE_COMPONENT));
    let description = ReleaseDescription {
        commit: COMMIT.to_owned(),
        components: vec![release_component()],
    };
    // The nested component is 223 bytes (0xdf), a two-byte varint length.
    let mut want = bytes(RELEASE_DESCRIPTION_COMMIT);
    want.extend(framed("12df01", RELEASE_COMPONENT));
    golden(&description, &want);
    // The request that asks for every complete platform names none.
    let all = DescribeRequest {
        commit: COMMIT.to_owned(),
        platform: String::new(),
    };
    golden(&all, &bytes(&DESCRIBE_REQUEST[..1]));
}

/// proto3 writes no field at its default: an empty message is no bytes, and a message that
/// sets only the zero values is the same bytes.
#[test]
fn defaults_are_not_written() {
    golden(&UpdateStatus::default(), &[]);
    golden(&UpdateOffer::default(), &[]);
    golden(&UpdateOrder::default(), &[]);
    golden(&DescribeRequest::default(), &[]);
    golden(&ReleaseComponent::default(), &[]);
    golden(&ReleaseDescription::default(), &[]);
    let zero = UpdateOrder {
        verb: Verb::Unspecified.into(),
        ..UpdateOrder::default()
    };
    assert_eq!(zero.encode_to_vec(), Vec::<u8>::new());
}

/// Catches `update_status` off field 7 of `DaemonMessage` (tag `0x3a`: 7 << 3 | 2), and
/// `update_offer` and `update_order` off fields 8 and 9 of `ServerMessage` (tags `0x42`
/// and `0x4a`), which would make every released node and frontend read the other's
/// messages as unknown fields.
#[test]
fn the_session_carries_the_messages_on_fields_7_8_and_9() {
    let from_node = DaemonMessage {
        message: Some(daemon_message::Message::UpdateStatus(status())),
    };
    // Length 0x74 = 116.
    golden(&from_node, &framed("3a74", STATUS));
    let offered = ServerMessage {
        message: Some(server_message::Message::UpdateOffer(offer())),
    };
    // Length 0xe501 = 229 (a two-byte varint).
    golden(&offered, &framed("42f401", OFFER));
    let ordered = ServerMessage {
        message: Some(server_message::Message::UpdateOrder(order())),
    };
    // Length 0x33 = 51.
    golden(&ordered, &framed("4a35", ORDER));
}

/// Catches an enum value renumbered: the numbers are on the wire, and a host that reads a
/// value it does not know treats the message as unspecified.
#[test]
fn enum_numbers_are_pinned() {
    for (verb, n) in [
        (Verb::Unspecified, 0),
        (Verb::Activate, 1),
        (Verb::Confirm, 2),
        (Verb::Revert, 3),
    ] {
        assert_eq!(verb as i32, n);
    }
    for (state, n) in [
        (State::Unspecified, 0),
        (State::Idle, 1),
        (State::Fetching, 2),
        (State::Staged, 3),
        (State::ActivatedUnconfirmed, 4),
        (State::Confirmed, 5),
        (State::Reverted, 6),
        (State::Refused, 7),
        (State::Manual, 8),
    ] {
        assert_eq!(state as i32, n);
    }
    for (source, n) in [
        (OrderSource::Unspecified, 0),
        (OrderSource::Fetch, 1),
        (OrderSource::Staged, 2),
    ] {
        assert_eq!(source as i32, n);
    }
    assert!(State::try_from(9).is_err());
    assert!(Verb::try_from(4).is_err());
    assert!(OrderSource::try_from(3).is_err());
}

/// Catches a service renamed: the name is the path every host dials.
#[test]
fn the_release_service_has_its_name() {
    assert_eq!(
        <ReleaseServiceServer<()> as NamedService>::NAME,
        "kbf.update.v1.ReleaseService"
    );
}
