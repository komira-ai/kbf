//! `kbf.enroll.v1`: the bytes are the contract between a frontend and every node it
//! enrolls, so they are pinned here by hand-assembled vectors, not by a round trip.
//!
//! The vectors were built independently of prost (a field-by-field varint and
//! length-prefix assembly from the proto's field numbers, in a script), so a renumbered
//! field, a changed type or a changed default shows up as a byte difference.
//!
//! Catches:
//! - a field number or wire type changed or reused (every request, response and
//!   `NodeConfig` field is in a golden message, none of them left at a default);
//! - a default value written to the wire, or a present optional (`RenewResponse.config`)
//!   not told apart from an absent one;
//! - a service name that differs from the one nodes dial;
//! - a `CredentialMode` that does not refuse zero and unknown values;
//! - a refusal code in `kbf_types` whose number is not `google.rpc.Code`'s;
//! - a token's id and secret not fitting the `EnrollRequest` fields they travel in.

use kbf_proto::google::rpc::Code;
use kbf_proto::kbf::enroll::v1::{
    CredentialMode, EnrollRequest, EnrollResponse, GetStoreConfigRequest, GetStoreConfigResponse,
    IssuedCert, NodeConfig, NodeFacts, RenewRequest, RenewResponse, RoleCsr, StoreConfig,
    cold_store_server::ColdStoreServer, enroll_server::EnrollServer,
};
use kbf_types::{CaPin, RefusalCode, RefusalReason, Token, TokenId, TokenSecret};
use prost::Message;
use tonic::server::NamedService;

fn bytes_of(hex_text: &str) -> Vec<u8> {
    hex::decode(hex_text).expect("golden hex")
}

fn csr(role: &str, der: &[u8]) -> RoleCsr {
    RoleCsr {
        role: role.to_owned(),
        csr_der: der.to_vec(),
    }
}

fn storage_cert() -> IssuedCert {
    IssuedCert {
        role: "storage".to_owned(),
        leaf_der: vec![0x30, 0x82],
        serial: "1f2e".to_owned(),
        not_before: 1_800_000_000,
        not_after: 1_800_604_800,
    }
}

fn config() -> NodeConfig {
    NodeConfig {
        generation: 5,
        roles: vec!["storage".to_owned(), "daemon".to_owned()],
        worker_endpoint: "https://f.example:8983".to_owned(),
        cas_endpoint: "https://f.example:8984".to_owned(),
        storage_port: 8985,
        cold_store: "s3://b/p/?endpoint=e".to_owned(),
        min_protocol: 3,
        tls_reload_interval_s: 30,
        frontend_url: "https://g.example:8990".to_owned(),
        credential_mode: CredentialMode::Ambient as i32,
    }
}

/// Asserts `message` encodes to `golden` and `golden` decodes to `message`.
fn golden<M: Message + Default + PartialEq + std::fmt::Debug>(message: &M, golden: &str) {
    let want = bytes_of(golden);
    assert_eq!(message.encode_to_vec(), want, "encoding");
    assert_eq!(
        &M::decode(want.as_slice()).expect("decode"),
        message,
        "decoding"
    );
}

const REQUEST: &str = "0a0c616572756b7a346a767067361220000102030405060708090a0b0c0d0e0f10111213141516171819\
1a1b1c1d1e1f1a076e6f64652d3032220c6e6f64652d30323a383938332a0161320e0a0773746f726167\
651203300102320c0a066461656d6f6e120230003a1b0a056c696e757812067838365f36341a03616263\
20022a03302e31";

const CONFIG: &str = "0805120773746f7261676512066461656d6f6e1a1668747470733a2f2f662e6578616d706c653a383938\
33221668747470733a2f2f662e6578616d706c653a38393834289946321473333a2f2f622f702f3f656e\
64706f696e743d653803401e4a1668747470733a2f2f672e6578616d706c653a383939305002";

const CERT: &str = "0a0773746f72616765120230821a04316632652080a4a7da06288099ccda06";

#[test]
fn an_enroll_request_is_golden() {
    let request = EnrollRequest {
        token_id: "aerukz4jvpg6".to_owned(),
        token_secret: (0..32).collect(),
        node_id: "node-02".to_owned(),
        advertise: "node-02:8983".to_owned(),
        zone: "a".to_owned(),
        csrs: vec![
            csr("storage", &[0x30, 0x01, 0x02]),
            csr("daemon", &[0x30, 0x00]),
        ],
        facts: Some(NodeFacts {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            commit: "abc".to_owned(),
            protocol: 2,
            package_version: "0.1".to_owned(),
        }),
    };
    golden(&request, REQUEST);
}

#[test]
fn an_enroll_response_is_golden() {
    let daemon_cert = IssuedCert {
        role: "daemon".to_owned(),
        leaf_der: vec![0x30],
        serial: "a".to_owned(),
        not_before: 0,
        not_after: 1,
    };
    let response = EnrollResponse {
        certs: vec![storage_cert(), daemon_cert],
        ca_bundle_pem: b"-----BEGIN CERTIFICATE-----\n".to_vec(),
        config: Some(config()),
        server_time: 1_800_000_100,
        renew_after: 1_800_302_400,
    };
    golden(
        &response,
        "0a1f0a0773746f72616765120230821a04316632652080a4a7da06288099ccda060a100a066461656d6f\
6e1201301a01612801121c2d2d2d2d2d424547494e2043455254494649434154452d2d2d2d2d0a1a7a08\
05120773746f7261676512066461656d6f6e1a1668747470733a2f2f662e6578616d706c653a38393833\
221668747470733a2f2f662e6578616d706c653a38393834289946321473333a2f2f622f702f3f656e64\
706f696e743d653803401e4a1668747470733a2f2f672e6578616d706c653a38393930500220e4a4a7da\
0628c0deb9da06",
    );
}

/// Catches: a default (here `not_before: 0`) written to the wire, which a reader cannot
/// tell from a field set on purpose, a negative time encoded as anything but ten bytes of
/// two's complement, and a time field narrowed to 32 bits.
#[test]
fn defaults_are_omitted_and_negatives_are_ten_bytes() {
    let cert = IssuedCert {
        not_before: -1,
        ..Default::default()
    };
    golden(&cert, "20ffffffffffffffffff01");
    golden(&IssuedCert::default(), "");
    // A time past the range of an int32 (the year 2096) keeps all of its bits.
    let far = IssuedCert {
        not_after: 5_000_000_000,
        ..Default::default()
    };
    golden(&far, "2880e497d012");
    let far_enroll = EnrollResponse {
        renew_after: 5_000_000_000,
        ..Default::default()
    };
    golden(&far_enroll, "2880e497d012");
    let far_renew = RenewResponse {
        renew_after: 5_000_000_000,
        ..Default::default()
    };
    golden(&far_renew, "2880e497d012");
    let far_renew_at = RenewRequest {
        issued_at: 5_000_000_000,
        ..Default::default()
    };
    golden(&far_renew_at, "1880e497d012");
    let far_time = EnrollResponse {
        server_time: 5_000_000_000,
        ..Default::default()
    };
    golden(&far_time, "2080e497d012");
}

#[test]
fn a_renew_request_is_golden() {
    let request = RenewRequest {
        node_id: "node-02".to_owned(),
        csrs: vec![csr("storage", &[0x30, 0x01, 0x02])],
        issued_at: 1_800_302_400,
        config_generation: 5,
    };
    golden(
        &request,
        "0a076e6f64652d3032120e0a0773746f72616765120330010218c0deb9da062005",
    );
}

/// Catches: `config` written when the node already holds the generation (the design
/// sends it only when it differs), or an absent config read back as a present empty one.
#[test]
fn a_renew_response_without_config_has_no_config_field() {
    let response = RenewResponse {
        certs: vec![storage_cert()],
        ca_bundle_pem: b"ca".to_vec(),
        config: None,
        server_time: 1_800_302_401,
        renew_after: 1_800_906_000,
    };
    golden(
        &response,
        &format!("0a1f{CERT}1202636120c1deb9da062890cadeda06"),
    );
    let with_empty = RenewResponse {
        config: Some(NodeConfig::default()),
        ..response
    };
    let wire = with_empty.encode_to_vec();
    assert_ne!(
        wire,
        bytes_of(&format!("0a1f{CERT}1202636120c1deb9da062890cadeda06"))
    );
    assert!(
        RenewResponse::decode(wire.as_slice())
            .unwrap()
            .config
            .is_some()
    );
}

#[test]
fn a_node_config_is_golden() {
    golden(&config(), CONFIG);
}

fn store_config() -> StoreConfig {
    StoreConfig {
        connection: "main".to_owned(),
        role: "cold".to_owned(),
        endpoint: "http://s3.example:9000".to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "bkt".to_owned(),
        prefix: "kbf/".to_owned(),
        conditional_put: true,
        min_put_rate: 1_048_576,
        access_key_id: "AKIAEXAMPLE".to_owned(),
        secret_access_key: "s3cr3t".to_owned(),
        secret_version: 3,
    }
}

const STORE_CONFIG: &str = "0a046d61696e1204636f6c641a16687474703a2f2f73332e6578616d706c653a39303030\
220975732d656173742d312a03626b7432046b62662f3801408080404a0b414b49414558414d504c4552\
067333637233745803";

/// Catches: a `StoreConfig` field renumbered or retyped, and a default written to the wire.
/// Every field is set, so each number is in the vector.
#[test]
fn the_registered_store_is_golden() {
    golden(&store_config(), STORE_CONFIG);
}

/// Catches: the request's generation moved off field 1, and an answer that carries the
/// store when it should carry only the generation (an unchanged answer is two bytes:
/// the secret is not in it).
#[test]
fn the_store_config_calls_are_golden_and_an_unchanged_answer_has_no_store() {
    golden(&GetStoreConfigRequest { have_generation: 7 }, "0807");
    golden(
        &GetStoreConfigResponse {
            generation: 7,
            config: None,
        },
        "0807",
    );
    golden(
        &GetStoreConfigResponse {
            generation: 7,
            config: Some(store_config()),
        },
        &format!("080712570a{}", &STORE_CONFIG[2..]),
    );
}

/// Catches: the secret reaching a log through `Debug` of the message, of the answer that
/// holds it, or of an error path that formats either; and a store that leaves its secret
/// readable in memory after it is dropped is covered by the `zeroize` call in its `Drop`.
#[test]
fn the_registered_stores_debug_prints_no_secret() {
    let config = store_config();
    let answer = GetStoreConfigResponse {
        generation: 7,
        config: Some(store_config()),
    };
    for text in [
        format!("{config:?}"),
        format!("{config:#?}"),
        format!("{answer:?}"),
        format!("{answer:#?}"),
    ] {
        assert!(!text.contains("s3cr3t"), "{text}");
        assert!(text.contains("Secret(..)"), "{text}");
        // The rest is there to read: the name, the endpoint and the key id are not secret.
        assert!(
            text.contains("main") && text.contains("AKIAEXAMPLE"),
            "{text}"
        );
    }
}

/// Catches: a credential mode with a reused number, a zero that is a real mode (an unset
/// field would pick how a node reaches the object store), and an unknown number that
/// decodes (a reader must be able to refuse it).
#[test]
fn credential_mode_is_closed() {
    assert_eq!(CredentialMode::Unspecified as i32, 0);
    assert_eq!(CredentialMode::Keys as i32, 1);
    assert_eq!(CredentialMode::Ambient as i32, 2);
    assert_eq!(CredentialMode::try_from(1).ok(), Some(CredentialMode::Keys));
    assert_eq!(
        CredentialMode::try_from(2).ok(),
        Some(CredentialMode::Ambient)
    );
    assert_eq!(
        CredentialMode::try_from(0).ok(),
        Some(CredentialMode::Unspecified)
    );
    assert!(CredentialMode::try_from(3).is_err());
    assert!(CredentialMode::try_from(-1).is_err());
}

/// Catches: a package or service renamed, which makes every node's call UNIMPLEMENTED.
#[test]
fn the_service_names_are_the_ones_nodes_dial() {
    assert_eq!(
        <EnrollServer<()> as NamedService>::NAME,
        "kbf.enroll.v1.Enroll"
    );
    assert_eq!(
        <ColdStoreServer<()> as NamedService>::NAME,
        "kbf.enroll.v1.ColdStore"
    );
}

/// Catches: a refusal code whose number is not the `google.rpc.Code` one (a node would
/// retry what it must not, or give up on what it should retry).
#[test]
fn refusal_codes_are_google_rpc_codes() {
    let pairs = [
        (RefusalCode::InvalidArgument, Code::InvalidArgument),
        (RefusalCode::Unauthenticated, Code::Unauthenticated),
        (RefusalCode::PermissionDenied, Code::PermissionDenied),
        (RefusalCode::AlreadyExists, Code::AlreadyExists),
        (RefusalCode::ResourceExhausted, Code::ResourceExhausted),
        (RefusalCode::Unavailable, Code::Unavailable),
    ];
    for (ours, theirs) in pairs {
        assert_eq!(ours.number(), theirs as i32, "{ours:?}");
    }
    for reason in RefusalReason::ALL {
        assert!(Code::try_from(reason.code().number()).is_ok(), "{reason}");
    }
}

/// Catches: a token whose id or secret cannot be carried in the request fields it is
/// sent in (the id as a string, the secret as exactly 32 bytes).
#[test]
fn a_token_fills_the_request_fields_it_travels_in() {
    let token = Token::new(
        TokenId::from_bits(0x0012_3456_789a_bcde),
        TokenSecret::from_bytes(std::array::from_fn(|i| i as u8)),
        CaPin::from_bytes([7; 32]),
    );
    let request = EnrollRequest {
        token_id: token.id().to_string(),
        token_secret: token.secret().as_bytes().to_vec(),
        ..Default::default()
    };
    let wire = request.encode_to_vec();
    // Fields 1 and 2 of the golden request, and nothing else.
    assert_eq!(wire, bytes_of(&REQUEST[..2 * (2 + 12 + 2 + 32)]));
}

/// Catches: a node that fails on an answer from a NEWER frontend (the frontend updates first,
/// so a node must accept an N+1 frontend). The store, its answer and the request carry extra
/// fields of an unknown number and wire type (a varint, a length-delimited one, a fixed
/// 32-bit one) at the end and in the middle; each decodes, to the fields this build knows,
/// and a generation this build has never held is just a number.
///
/// Mutant seen red: a `deny_unknown_fields`-style strict decode (a hand check that the
/// re-encoded bytes equal the input).
#[test]
fn a_newer_frontends_store_config_answer_decodes() {
    let unknown = [0x78, 0x01, 0x72, 0x02, b'h', b'i', 0xad, 0x01, 1, 2, 3, 4];
    let mut store = store_config().encode_to_vec();
    // After field 1 (`0a 04 main`, six bytes), so the unknown fields sit between known ones.
    store.splice(6..6, unknown.iter().copied());
    store.extend_from_slice(&unknown);
    // The store is longer than 127 bytes, so its length is a two-byte varint.
    let mut framed = vec![0x08, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x12];
    let mut len = store.len();
    while len >= 0x80 {
        framed.push((len & 0x7f) as u8 | 0x80);
        len >>= 7;
    }
    framed.push(len as u8);
    framed.extend_from_slice(&store);
    framed.extend_from_slice(&unknown);
    let got = GetStoreConfigResponse::decode(framed.as_slice()).expect("decodes");
    assert_eq!(got.generation, u64::from(u32::MAX));
    assert_eq!(got.config, Some(store_config()));
    let request = [&[0x08, 0x07][..], &unknown].concat();
    assert_eq!(
        GetStoreConfigRequest::decode(request.as_slice()).expect("decodes"),
        GetStoreConfigRequest { have_generation: 7 }
    );
}
