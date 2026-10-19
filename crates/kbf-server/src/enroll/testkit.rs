//! Fixtures the enroll tests share: a farm (a state store with a clock the test moves, a
//! CA made in a scratch directory, the service over both), tokens, and certificate requests
//! built the way a node builds them (and some it would not).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use kbf_ca::{CLOCK_FLOOR_UNIX, ISSUED_AT_OID, Policy, SystemEntropy};
use kbf_enroll_registry::{TokenSpec, create_token};
use kbf_proto::kbf::enroll::v1 as pb;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{NodeId, Role, RoleSet, TokenId, TokenSecret};
use rcgen::{Attribute, CertificateParams, DistinguishedName, KeyPair};

use super::ca::{Ca, SignerChoice};
use super::service::{EnrollService, NodeSettings};

/// The store's clock at the start of a test: a day after the CA's clock floor.
pub const START: i64 = CLOCK_FLOOR_UNIX + 86_400;
/// The one name this test frontend serves.
pub const FRONTEND: &str = "front.test";

/// A scratch directory no other test uses (a path that does not exist yet).
pub fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "kbf-enroll-svc-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A state store, a CA and the service over them.
pub struct Farm {
    pub clock: Arc<ManualClock>,
    pub store: Arc<dyn Store>,
    pub ca: Arc<Ca>,
    pub service: EnrollService,
}

/// A farm whose store clock reads `start` (unix seconds) and whose CA, made in a new
/// directory, was made at `START`.
pub fn farm_at(name: &str, start: i64) -> Farm {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(u64::try_from(start).expect("a time")));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let dir = scratch(name);
    let policy = Policy::new(86_400).expect("policy");
    let ca = Arc::new(
        Ca::open(
            &dir,
            &SignerChoice::DevFile,
            policy,
            START,
            &mut SystemEntropy,
        )
        .expect("a CA"),
    );
    let service = service_over(&store, &ca);
    Farm {
        clock,
        store,
        ca,
        service,
    }
}

/// A farm at `START`.
pub fn farm(name: &str) -> Farm {
    farm_at(name, START)
}

/// The service over `store` and `ca`.
pub fn service_over(store: &Arc<dyn Store>, ca: &Arc<Ca>) -> EnrollService {
    EnrollService::new(
        Arc::clone(store),
        Arc::clone(ca),
        &[FRONTEND.to_owned()],
        NodeSettings {
            worker_endpoint: "https://front.test:7443".to_owned(),
            cas_endpoint: String::new(),
            storage_port: 8983,
        },
    )
}

impl Farm {
    /// Moves the store's clock (and so the time certificates are issued at).
    pub fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }

    /// The store's time in unix seconds.
    pub fn now(&self) -> i64 {
        i64::try_from(self.store.now().as_duration().as_secs()).expect("time")
    }
}

/// A token spec granting `roles`, one use, with no node id, prefix or names.
pub fn spec(roles: &[Role]) -> TokenSpec {
    TokenSpec {
        roles: RoleSet::new(roles.iter().copied()).expect("roles"),
        max_uses: 1,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "test".to_owned(),
        created_by: "test".to_owned(),
    }
}

/// A token the store holds; its id and secret.
pub fn token(farm: &Farm, bits: u64, spec: &TokenSpec) -> (TokenId, TokenSecret) {
    let id = TokenId::from_bits(bits);
    #[allow(clippy::cast_possible_truncation)]
    let secret = TokenSecret::from_bytes([bits as u8; 32]);
    create_token(&*farm.store, &id, &secret, spec).expect("token");
    (id, secret)
}

/// A new P-256 key.
pub fn key() -> KeyPair {
    KeyPair::generate().expect("key")
}

/// The attribute `issued_at` of a renewal request (a SET of one OCTET STRING of 8 bytes).
fn issued_at_attribute(t: i64) -> Attribute {
    let mut value = vec![0x31, 0x0a, 0x04, 0x08];
    value.extend_from_slice(&t.to_be_bytes());
    Attribute {
        oid: ISSUED_AT_OID,
        values: value,
    }
}

/// A node's CSR for `key`, with `issued_at` among the signed attributes if given.
pub fn csr(key: &KeyPair, issued_at: Option<i64>) -> Vec<u8> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    let attributes = issued_at.map(issued_at_attribute).into_iter().collect();
    params
        .serialize_request_with_attributes(key, attributes)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec()
}

/// A CSR whose signature does not verify: the last byte of a good one is changed.
pub fn csr_bad_signature(key: &KeyPair) -> Vec<u8> {
    let mut der = csr(key, None);
    *der.last_mut().expect("bytes") ^= 0x01;
    der
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(u8::try_from(n).expect("short"));
    } else {
        let digits: Vec<u8> = n
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | u8::try_from(digits.len()).expect("a length of few bytes"));
        out.extend(digits);
    }
    out.extend_from_slice(content);
    out
}

/// A well-formed PKCS #10 request for an RSA key (the key is a made-up modulus; the
/// signature is not one): what the CA must refuse because of the key type, before it
/// looks at anything else.
pub fn csr_rsa() -> Vec<u8> {
    let rsa_oid = [
        0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
    ];
    let algorithm = tlv(0x30, &[rsa_oid.as_slice(), &[0x05, 0x00]].concat());
    let key = tlv(
        0x03,
        &[
            &[0u8][..],
            &[0x30, 0x06, 0x02, 0x01, 0x03, 0x02, 0x01, 0x03][..],
        ]
        .concat(),
    );
    let spki = tlv(0x30, &[algorithm.clone(), key].concat());
    let cri = tlv(
        0x30,
        &[tlv(0x02, &[0]), tlv(0x30, &[]), spki, tlv(0xa0, &[])].concat(),
    );
    let sha256_rsa = tlv(
        0x30,
        &[
            [
                0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b,
            ]
            .as_slice(),
            &[0x05, 0x00],
        ]
        .concat(),
    );
    let signature = tlv(0x03, &[&[0u8][..], &[7u8; 256][..]].concat());
    tlv(0x30, &[cri, sha256_rsa, signature].concat())
}

/// A CSR over the size the CA reads.
pub fn csr_oversized(key: &KeyPair) -> Vec<u8> {
    let mut der = csr(key, None);
    der.resize(kbf_ca::MAX_CSR_LEN + 1, 0);
    der
}

/// An enrollment request for `node` with a fresh key and a CSR for each of `roles`; the
/// keys come back in the same order.
pub fn enroll_request(
    token: &(TokenId, TokenSecret),
    node: &str,
    roles: &[Role],
    advertise: &str,
) -> (pb::EnrollRequest, Vec<KeyPair>) {
    let keys: Vec<KeyPair> = roles.iter().map(|_| key()).collect();
    let csrs = roles
        .iter()
        .zip(&keys)
        .map(|(role, key)| pb::RoleCsr {
            role: role.name().to_owned(),
            csr_der: csr(key, None),
        })
        .collect();
    let request = pb::EnrollRequest {
        token_id: token.0.to_string(),
        token_secret: token.1.as_bytes().to_vec(),
        node_id: node.to_owned(),
        zone: String::new(),
        advertise: advertise.to_owned(),
        csrs,
        ..Default::default()
    };
    (request, keys)
}

/// A renewal request for `node` from `keys`, made at `issued_at`.
pub fn renew_request(
    node: &str,
    roles: &[Role],
    keys: &[KeyPair],
    issued_at: i64,
) -> pb::RenewRequest {
    pb::RenewRequest {
        node_id: node.to_owned(),
        csrs: roles
            .iter()
            .zip(keys)
            .map(|(role, key)| pb::RoleCsr {
                role: role.name().to_owned(),
                csr_der: csr(key, Some(issued_at)),
            })
            .collect(),
        issued_at,
        ..Default::default()
    }
}

/// A node id.
pub fn node(text: &str) -> NodeId {
    text.parse().expect("node id")
}
