//! Helpers the integration tests share: deterministic randomness, a fake external signer,
//! a DER builder for requests the node's own builder would never make, and a reader that
//! reduces a certificate to the fields the design's table names.

#![allow(dead_code)]

pub mod answer;

use std::collections::VecDeque;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kbf_ca::{Algorithm, Custody, Entropy, PublicKey, SignError, Signer};
use rcgen::{Attribute, CertificateParams, DistinguishedName, KeyPair, PublicKeyData, SigningKey};
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

/// A clock reading well past the build's floor: 2027-01-15.
pub const NOW: i64 = 1_800_000_000;

/// A splitmix64 stream: deterministic, and different for every seed.
pub struct Stream(pub u64);

impl Entropy for Stream {
    fn fill(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
    }
}

/// Entropy that answers the first calls with the blocks given, then a stream.
pub struct Script {
    blocks: VecDeque<Vec<u8>>,
    rest: Stream,
}

impl Script {
    pub fn new(blocks: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            blocks: blocks.into_iter().collect(),
            rest: Stream(1),
        }
    }
}

impl Entropy for Script {
    fn fill(&mut self, dest: &mut [u8]) {
        match self.blocks.pop_front() {
            Some(block) => {
                assert_eq!(
                    block.len(),
                    dest.len(),
                    "a scripted block is one whole draw"
                );
                dest.copy_from_slice(&block);
            }
            None => self.rest.fill(dest),
        }
    }
}

/// A signer whose key is "elsewhere": reports external custody, can be told to fail, and
/// counts the signatures it made.
pub struct External {
    key: KeyPair,
    spki: Vec<u8>,
    pub fail: AtomicBool,
    pub signed: AtomicUsize,
}

impl External {
    pub fn new() -> Arc<Self> {
        let key = KeyPair::generate().expect("key");
        let spki = key.subject_public_key_info();
        Arc::new(Self {
            key,
            spki,
            fail: AtomicBool::new(false),
            signed: AtomicUsize::new(0),
        })
    }
}

impl Signer for External {
    fn public_key(&self) -> &[u8] {
        &self.spki
    }

    fn custody(&self) -> Custody {
        Custody::External
    }

    fn sign(&self, tbs: &[u8], algorithm: Algorithm) -> Result<Vec<u8>, SignError> {
        assert_eq!(algorithm, Algorithm::EcdsaP256Sha256);
        if self.fail.load(Ordering::SeqCst) {
            return Err(SignError("signer offline".to_owned()));
        }
        self.signed.fetch_add(1, Ordering::SeqCst);
        Ok(SigningKey::sign(&self.key, tbs).expect("sign"))
    }
}

/// A scratch directory, new and empty, resolved through symlinks.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-ca-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::canonicalize(&dir).expect("canonical scratch dir")
}

/// The key of a freshly generated rcgen key pair.
pub fn public_of(key: &KeyPair) -> PublicKey {
    PublicKey::from_spki(&key.subject_public_key_info()).expect("a P-256 key")
}

/// The request the node's own builder makes (`kbf-enroll`'s `csr::request`): an empty
/// subject, no extensions, `issued_at` as a signed attribute when given.
pub fn client_csr(key: &KeyPair, issued_at: Option<i64>) -> Vec<u8> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    let attributes = issued_at
        .map(|t| {
            let mut value = vec![0x31, 0x0a, 0x04, 0x08];
            value.extend_from_slice(&t.to_be_bytes());
            Attribute {
                oid: kbf_ca::ISSUED_AT_OID,
                values: value,
            }
        })
        .into_iter()
        .collect();
    params
        .serialize_request_with_attributes(key, attributes)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec()
}

// ---- DER by hand --------------------------------------------------------------------

/// One DER element.
pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let n = content.len();
    let mut out = vec![tag];
    if n < 0x80 {
        out.push(u8::try_from(n).expect("short"));
    } else if n < 0x100 {
        out.extend([0x81, u8::try_from(n).expect("one byte")]);
    } else {
        out.extend([
            0x82,
            u8::try_from(n >> 8).expect("two bytes"),
            (n & 0xff) as u8,
        ]);
    }
    out.extend_from_slice(content);
    out
}

pub fn seq(content: &[u8]) -> Vec<u8> {
    tlv(0x30, content)
}

pub fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// `ecdsa-with-SHA256` and `ecdsa-with-SHA384` as the content of an AlgorithmIdentifier.
pub const SHA256_ALG: [u8; 10] = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
pub const SHA384_ALG: [u8; 10] = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
/// `id-ecPublicKey` and `prime256v1`.
pub const EC_PUBLIC_KEY: [u8; 9] = [0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
pub const PRIME256V1: [u8; 10] = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// `rsaEncryption` with its NULL parameter.
pub const RSA_ALG: [u8; 13] = [
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];

/// A SubjectPublicKeyInfo of P-256 around any bytes given as the point.
pub fn spki_p256(point: &[u8]) -> Vec<u8> {
    spki_with(&cat(&[&EC_PUBLIC_KEY, &PRIME256V1]), 0, point)
}

/// A SubjectPublicKeyInfo of `algorithm` (the content of its AlgorithmIdentifier) around
/// `key`, with `unused` bits declared.
pub fn spki_with(algorithm: &[u8], unused: u8, key: &[u8]) -> Vec<u8> {
    let mut bits = vec![unused];
    bits.extend_from_slice(key);
    seq(&cat(&[&seq(algorithm), &tlv(0x03, &bits)]))
}

/// A subject of one common name.
pub fn subject(cn: &str) -> Vec<u8> {
    let atv = seq(&cat(&[
        &[0x06, 0x03, 0x55, 0x04, 0x03],
        &tlv(0x0c, cn.as_bytes()),
    ]));
    seq(&tlv(0x31, &atv))
}

/// An attribute: its OID content and the full SET.
pub fn attribute(oid_content: &[u8], set: &[u8]) -> Vec<u8> {
    seq(&cat(&[&tlv(0x06, oid_content), set]))
}

/// The content of the `issued_at` OID `2.25.<0x6b62666e65776c>`, encoded by hand.
pub fn issued_at_oid_content() -> Vec<u8> {
    let arcs: Vec<u64> = kbf_ca::ISSUED_AT_OID.to_vec();
    let mut out = vec![u8::try_from(arcs[0] * 40 + arcs[1]).expect("first byte")];
    for arc in &arcs[2..] {
        let mut groups = Vec::new();
        let mut v = *arc;
        loop {
            groups.push((v & 0x7f) as u8);
            v >>= 7;
            if v == 0 {
                break;
            }
        }
        groups.reverse();
        let last = groups.len() - 1;
        out.extend(
            groups
                .iter()
                .enumerate()
                .map(|(i, g)| if i < last { g | 0x80 } else { *g }),
        );
    }
    out
}

/// A well-formed `issued_at` attribute for `t`.
pub fn issued_at_attribute(t: i64) -> Vec<u8> {
    attribute(
        &issued_at_oid_content(),
        &tlv(0x31, &tlv(0x04, &t.to_be_bytes())),
    )
}

/// How a request's signature is made.
pub enum Sig<'a> {
    /// By this key, over the request info.
    By(&'a KeyPair),
    /// These bytes, whatever they sign.
    Raw(Vec<u8>),
}

/// A PKCS #10 request made by hand; every part can be made wrong.
pub struct Hand {
    pub outer_tag: u8,
    pub cri_tag: u8,
    pub version_tag: u8,
    pub version: Vec<u8>,
    pub version_raw: Option<Vec<u8>>,
    pub subject: Vec<u8>,
    pub spki: Vec<u8>,
    pub attrs_tag: u8,
    pub attrs: Vec<Vec<u8>>,
    pub no_attrs: bool,
    pub extra_in_cri: Vec<u8>,
    pub sig_alg: Vec<u8>,
    pub sig_alg_tag: u8,
    pub sig_tag: u8,
    pub sig_unused: u8,
    pub after_sig: Vec<u8>,
}

impl Hand {
    /// A valid request for `key`, once built with a signature by it.
    pub fn new(key: &KeyPair) -> Self {
        Self {
            outer_tag: 0x30,
            cri_tag: 0x30,
            version_tag: 0x02,
            version: vec![0],
            version_raw: None,
            subject: subject("whoever"),
            spki: key.subject_public_key_info(),
            attrs_tag: 0xa0,
            attrs: Vec::new(),
            no_attrs: false,
            extra_in_cri: Vec::new(),
            sig_alg: SHA256_ALG.to_vec(),
            sig_alg_tag: 0x30,
            sig_tag: 0x03,
            sig_unused: 0,
            after_sig: Vec::new(),
        }
    }

    pub fn cri(&self) -> Vec<u8> {
        let attrs = if self.no_attrs {
            Vec::new()
        } else {
            tlv(self.attrs_tag, &self.attrs.concat())
        };
        tlv(
            self.cri_tag,
            &cat(&[
                &self
                    .version_raw
                    .clone()
                    .unwrap_or_else(|| tlv(self.version_tag, &self.version)),
                &self.subject,
                &self.spki,
                &attrs,
                &self.extra_in_cri,
            ]),
        )
    }

    pub fn build(&self, sig: &Sig<'_>) -> Vec<u8> {
        let cri = self.cri();
        let signature = match sig {
            Sig::By(key) => SigningKey::sign(*key, &cri).expect("sign"),
            Sig::Raw(bytes) => bytes.clone(),
        };
        let mut bits = vec![self.sig_unused];
        bits.extend_from_slice(&signature);
        tlv(
            self.outer_tag,
            &cat(&[
                &cri,
                &tlv(self.sig_alg_tag, &self.sig_alg),
                &tlv(self.sig_tag, &bits),
                &self.after_sig,
            ]),
        )
    }

    pub fn signed(&self, key: &KeyPair) -> Vec<u8> {
        self.build(&Sig::By(key))
    }
}

// ---- a certificate reduced to what the table names -----------------------------------

/// What the design's table says a certificate holds, read back from its DER.
#[derive(Debug)]
pub struct View {
    pub serial_hex: String,
    pub not_before: i64,
    pub not_after: i64,
    pub spki: Vec<u8>,
    pub cn: Vec<String>,
    pub uris: Vec<String>,
    pub dns: Vec<String>,
    pub ips: Vec<IpAddr>,
    pub other_sans: usize,
    pub server_auth: bool,
    pub client_auth: bool,
    pub other_ekus: usize,
    pub ca: Option<(bool, bool, Option<u32>)>,
    pub key_usage: Option<(bool, u16)>,
    pub extension_oids: Vec<String>,
    pub subject_key_id: Option<Vec<u8>>,
    pub authority_key_id: Option<Vec<u8>>,
    pub issuer: String,
    pub subject: String,
}

pub fn view(der: &[u8]) -> View {
    let (rest, cert) = X509Certificate::from_der(der).expect("a certificate");
    assert!(rest.is_empty(), "nothing after the certificate");
    assert_eq!(cert.version().0, 2, "X.509 v3");
    let mut uris = Vec::new();
    let mut dns = Vec::new();
    let mut ips = Vec::new();
    let mut other_sans = 0;
    if let Some(san) = cert.subject_alternative_name().expect("san parses") {
        for name in &san.value.general_names {
            match name {
                GeneralName::URI(u) => uris.push((*u).to_owned()),
                GeneralName::DNSName(d) => dns.push((*d).to_owned()),
                GeneralName::IPAddress(b) => ips.push(match <[u8; 4]>::try_from(*b) {
                    Ok(v4) => IpAddr::from(v4),
                    Err(_) => IpAddr::from(<[u8; 16]>::try_from(*b).expect("16 bytes")),
                }),
                _ => other_sans += 1,
            }
        }
    }
    let eku = cert.extended_key_usage().expect("eku parses");
    let (server_auth, client_auth, other_ekus) = eku.map_or((false, false, 0), |e| {
        let v = e.value;
        (
            v.server_auth,
            v.client_auth,
            usize::from(v.any)
                + usize::from(v.code_signing)
                + usize::from(v.email_protection)
                + usize::from(v.time_stamping)
                + usize::from(v.ocsp_signing)
                + v.other.len(),
        )
    });
    let ca = cert
        .basic_constraints()
        .expect("basic constraints parse")
        .map(|b| (b.critical, b.value.ca, b.value.path_len_constraint));
    let key_usage = cert
        .key_usage()
        .expect("key usage parses")
        .map(|k| (k.critical, k.value.flags));
    let mut subject_key_id = None;
    let mut authority_key_id = None;
    for ext in cert.iter_extensions() {
        match ext.parsed_extension() {
            x509_parser::extensions::ParsedExtension::SubjectKeyIdentifier(k) => {
                subject_key_id = Some(k.0.to_vec());
            }
            x509_parser::extensions::ParsedExtension::AuthorityKeyIdentifier(a) => {
                authority_key_id = a.key_identifier.as_ref().map(|k| k.0.to_vec());
            }
            _ => {}
        }
    }
    let mut extension_oids: Vec<String> = cert
        .iter_extensions()
        .map(|e| e.oid.to_id_string())
        .collect();
    extension_oids.sort();
    View {
        serial_hex: cert.serial.to_str_radix(16),
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        spki: cert.public_key().raw.to_vec(),
        cn: cert
            .subject()
            .iter_common_name()
            .map(|c| c.as_str().expect("utf8").to_owned())
            .collect(),
        uris,
        dns,
        ips,
        other_sans,
        server_auth,
        client_auth,
        other_ekus,
        ca,
        key_usage,
        extension_oids,
        subject_key_id,
        authority_key_id,
        issuer: cert.issuer().to_string(),
        subject: cert.subject().to_string(),
    }
}

/// The extension OIDs of the design's table for an end-entity certificate: subject key id,
/// key usage, subject alternative name, basic constraints, authority key id, EKU.
pub fn table_extension_oids() -> Vec<String> {
    // 14 subjectKeyIdentifier, 15 keyUsage, 17 subjectAltName, 19 basicConstraints, 35
    // authorityKeyIdentifier, 37 extKeyUsage: all under id-ce, 2.5.29.
    let mut oids: Vec<String> = [14, 15, 17, 19, 35, 37]
        .iter()
        .map(|n| format!("2.5.29.{n}"))
        .collect();
    oids.sort();
    oids
}

/// `KeyUsage.flags` bits: digitalSignature is bit 0, keyCertSign bit 5, cRLSign bit 6.
pub const KU_DIGITAL_SIGNATURE: u16 = 1;
pub const KU_KEY_CERT_SIGN: u16 = 1 << 5;
pub const KU_CRL_SIGN: u16 = 1 << 6;

pub fn pem_of(der: &[u8]) -> String {
    kbf_ca::Bundle::new(vec![der.to_vec()])
        .expect("a certificate")
        .to_pem()
}
