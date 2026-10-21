//! Certificate requests: what `parse_csr` takes, and every way it refuses.
//!
//! The request is the one thing on the enroll listener an anonymous caller writes, so each
//! refusal below is a defect that would let a caller's bytes reach the CA.

mod support;

use kbf_ca::{CsrError, MAX_CSR_LEN, parse_csr};
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P384_SHA384, PKCS_ED25519,
    PublicKeyData, SanType,
};
use support::{
    Hand, RSA_ALG, SHA384_ALG, Sig, attribute, cat, client_csr, issued_at_attribute,
    issued_at_oid_content, public_of, seq, spki_p256, spki_with, subject, tlv,
};

fn key() -> KeyPair {
    KeyPair::generate().expect("key")
}

// Catches: a parser that rejects the request the node's own builder makes, or that returns
// some other key than the one the request proves possession of.
#[test]
fn the_nodes_own_request_yields_its_key_and_no_time() {
    let key = key();
    let parsed = parse_csr(&client_csr(&key, None)).expect("a valid request");
    assert_eq!(parsed.key, public_of(&key));
    assert_eq!(parsed.key.spki(), key.subject_public_key_info());
    assert_eq!(parsed.key.point().as_slice(), key.public_key_raw());
    assert_eq!(parsed.issued_at, None);
}

// Catches: a wrong OID encoding for `issued_at` (a renewal would never carry a time), a
// byte-order slip, or a sign slip at the extremes of the range.
#[test]
fn issued_at_is_read_back_as_the_node_wrote_it() {
    let key = key();
    for t in [
        0,
        1,
        1_800_000_000,
        -5,
        i64::MAX,
        i64::MIN,
        0x0102_0304_0506_0708,
    ] {
        let parsed = parse_csr(&client_csr(&key, Some(t))).expect("a valid request");
        assert_eq!(parsed.issued_at, Some(t), "{t}");
    }
}

// Catches: reading the time from outside the signed part, or not verifying the signature at
// all: a replayed request with a fresh time would pass as a new one.
#[test]
fn a_request_whose_time_was_changed_after_signing_is_refused() {
    let key = key();
    let mut der = client_csr(&key, Some(1_800_000_000));
    let time = 1_800_000_000_i64.to_be_bytes();
    let at = der
        .windows(8)
        .position(|w| w == time)
        .expect("the time attribute");
    der[at + 7] ^= 1;
    assert_eq!(parse_csr(&der), Err(CsrError::Signature));
}

// Catches: a hand-written parser that trusts a time it cannot read: each of these is a
// wrong shape for the attribute, correctly signed, so only the shape check can refuse it.
#[test]
fn a_malformed_or_repeated_issued_at_is_refused() {
    let key = key();
    let oid = issued_at_oid_content();
    let good_set = tlv(0x31, &tlv(0x04, &[0; 8]));
    let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
        (
            "seven bytes",
            vec![attribute(&oid, &tlv(0x31, &tlv(0x04, &[0; 7])))],
        ),
        (
            "nine bytes",
            vec![attribute(&oid, &tlv(0x31, &tlv(0x04, &[0; 9])))],
        ),
        (
            "an integer",
            vec![attribute(&oid, &tlv(0x31, &tlv(0x02, &[0; 8])))],
        ),
        (
            "two values",
            vec![attribute(
                &oid,
                &tlv(0x31, &cat(&[&tlv(0x04, &[0; 8]), &tlv(0x04, &[0; 8])])),
            )],
        ),
        ("no value", vec![attribute(&oid, &tlv(0x31, &[]))]),
        (
            "trailing bytes in the value",
            vec![attribute(
                &oid,
                &tlv(0x31, &cat(&[&tlv(0x04, &[0; 8]), &[0]])),
            )],
        ),
        (
            "twice",
            vec![attribute(&oid, &good_set), attribute(&oid, &good_set)],
        ),
    ];
    for (what, attrs) in cases {
        let mut hand = Hand::new(&key);
        hand.attrs = attrs;
        assert_eq!(
            parse_csr(&hand.signed(&key)),
            Err(CsrError::IssuedAt),
            "{what}"
        );
    }
}

// Catches: a parser that reads attributes it has no use for (the extension request, a
// challenge password, anything else) and so lets them fail or shape the result.
#[test]
fn other_attributes_are_skipped_whatever_they_hold() {
    let key = key();
    let mut hand = Hand::new(&key);
    hand.attrs = vec![
        attribute(
            &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x0e],
            &tlv(0x31, &[0xff, 0xff]),
        ),
        attribute(&[0x55, 0x04, 0x03], &tlv(0x31, &[])),
        issued_at_attribute(7),
    ];
    let parsed = parse_csr(&hand.signed(&key)).expect("valid");
    assert_eq!(parsed.issued_at, Some(7));
    assert_eq!(parsed.key, public_of(&key));
}

// Catches: a parser that reads the subject: it is the caller's text, covered by the
// signature, and nothing the CA uses.
#[test]
fn the_subject_is_skipped_unread() {
    let key = key();
    let mut hand = Hand::new(&key);
    hand.subject = seq(&[0xff, 0xff, 0xff]);
    assert!(parse_csr(&hand.signed(&key)).is_ok());
}

// Catches: copying anything of a request but its key. A request that asks for a CA, a
// wildcard, another role, usages and a custom extension parses to the key and nothing else.
#[test]
fn a_hostile_request_yields_only_its_key() {
    let key = key();
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "frontend");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.subject_alt_names = vec![
        SanType::DnsName("*.example.com".try_into().expect("ia5")),
        SanType::URI("urn:kbf:role:frontend".try_into().expect("ia5")),
    ];
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.custom_extensions = vec![CustomExtension::from_oid_content(&[1, 2, 3, 4], vec![5])];
    let der = params
        .serialize_request(&key)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec();
    let parsed = parse_csr(&der).expect("valid");
    assert_eq!(parsed.key, public_of(&key));
    assert_eq!(parsed.issued_at, None);
}

// Catches: accepting a key type other than P-256 (one verifier set, one key type in the
// registry): RSA, P-384, Ed25519, and the shapes of a P-256 key that are not the one.
#[test]
fn only_an_uncompressed_p256_key_is_taken() {
    let signer = key();
    let point = signer.public_key_raw().to_vec();

    let p384 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("p384");
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    let p384_csr = params
        .serialize_request(&p384)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec();
    assert_eq!(parse_csr(&p384_csr), Err(CsrError::KeyType), "P-384");

    let ed = KeyPair::generate_for(&PKCS_ED25519).expect("ed25519");
    let ed_csr = params
        .serialize_request(&ed)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec();
    assert_eq!(parse_csr(&ed_csr), Err(CsrError::KeyType), "Ed25519");

    let mut compressed = point.clone();
    compressed[0] = 0x03;
    compressed.truncate(33);
    let ec = cat(&[&support::EC_PUBLIC_KEY, &support::PRIME256V1]);
    let mut other_curve = ec.clone();
    other_curve.truncate(support::EC_PUBLIC_KEY.len());
    other_curve.extend_from_slice(&[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22]);
    let trailing_in_spki = {
        let mut bits = vec![0];
        bits.extend_from_slice(&point);
        seq(&cat(&[&seq(&ec), &tlv(0x03, &bits), &[0x05, 0x00]]))
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "RSA",
            spki_with(&RSA_ALG, 0, &[0x30, 0x03, 0x02, 0x01, 0x01]),
        ),
        ("compressed point", spki_with(&ec, 0, &compressed)),
        ("a short point", spki_with(&ec, 0, &point[..64])),
        ("a long point", spki_with(&ec, 0, &cat(&[&point, &[0]]))),
        (
            "a point of 04 declared with unused bits",
            spki_with(&ec, 1, &point),
        ),
        (
            "a bad point prefix",
            spki_with(&ec, 0, &cat(&[&[0x05], &point[1..]])),
        ),
        ("secp384r1 named", spki_with(&other_curve, 0, &point)),
        ("trailing bytes in the key info", trailing_in_spki),
        ("an empty bit string", spki_with(&ec, 0, &[])),
        (
            "an algorithm that is not a sequence",
            seq(&cat(&[&tlv(0x31, &ec), &tlv(0x03, &[0])])),
        ),
        (
            "a key that is not a bit string",
            seq(&cat(&[&seq(&ec), &tlv(0x04, &point)])),
        ),
    ];
    for (what, spki) in cases {
        let mut hand = Hand::new(&signer);
        hand.spki = spki;
        assert_eq!(
            parse_csr(&hand.signed(&signer)),
            Err(CsrError::KeyType),
            "{what}"
        );
    }
}

// Catches: ring's check of the point being skipped: 04 and 64 bytes that are not on the
// curve must not pass for a key.
#[test]
fn a_point_off_the_curve_fails_the_proof_of_possession() {
    let signer = key();
    let mut hand = Hand::new(&signer);
    hand.spki = spki_p256(&cat(&[&[0x04], &[0x11; 64]]));
    assert_eq!(parse_csr(&hand.signed(&signer)), Err(CsrError::Signature));
}

// Catches: no proof of possession: a request for somebody else's key, a signature that does
// not match, a signature in the wrong encoding, and a request body changed after signing.
#[test]
fn a_request_that_does_not_prove_possession_is_refused() {
    let mine = key();
    let theirs = key();

    let stolen = Hand::new(&theirs).signed(&mine);
    assert_eq!(
        parse_csr(&stolen),
        Err(CsrError::Signature),
        "another key's"
    );

    let mut flipped = Hand::new(&mine).signed(&mine);
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert_eq!(
        parse_csr(&flipped),
        Err(CsrError::Signature),
        "a flipped bit"
    );

    let mut edited = Hand::new(&mine).signed(&mine);
    let at = edited
        .windows(7)
        .position(|w| w == b"whoever")
        .expect("the subject text");
    edited[at] = b'W';
    assert_eq!(
        parse_csr(&edited),
        Err(CsrError::Signature),
        "an edited subject"
    );

    let raw = Hand::new(&mine).build(&Sig::Raw(vec![7; 64]));
    assert_eq!(
        parse_csr(&raw),
        Err(CsrError::Signature),
        "not a DER signature"
    );

    let empty = Hand::new(&mine).build(&Sig::Raw(Vec::new()));
    assert_eq!(
        parse_csr(&empty),
        Err(CsrError::Signature),
        "an empty signature"
    );
}

// Catches: accepting a digest other than SHA-256, or an algorithm identifier with extras.
#[test]
fn only_ecdsa_with_sha256_is_a_signature_algorithm() {
    let key = key();
    for (what, alg) in [
        ("sha384", SHA384_ALG.to_vec()),
        (
            "sha256 with a parameter",
            cat(&[&support::SHA256_ALG, &[0x05, 0x00]]),
        ),
        ("empty", Vec::new()),
    ] {
        let mut hand = Hand::new(&key);
        hand.sig_alg = alg;
        assert_eq!(
            parse_csr(&hand.signed(&key)),
            Err(CsrError::SignatureAlgorithm),
            "{what}"
        );
    }
}

// Catches: a version other than 0 passing (a v2 request has fields this parser does not
// read).
#[test]
fn only_version_zero_is_accepted() {
    let key = key();
    for version in [vec![1], vec![2], vec![0, 0], vec![0xff], Vec::new()] {
        let mut hand = Hand::new(&key);
        hand.version = version.clone();
        assert_eq!(
            parse_csr(&hand.signed(&key)),
            Err(CsrError::Version),
            "{version:?}"
        );
    }
}

// Catches: a parser that panics, loops or accepts on bytes that are not a request, on a
// request cut anywhere, and on every encoding the DER rules forbid (non-minimal lengths,
// indefinite lengths, wrong tags, a missing or repeated part).
#[test]
fn bytes_that_are_not_a_request_are_refused_never_a_panic() {
    let key = key();
    let good = client_csr(&key, Some(5));
    assert!(parse_csr(&good).is_ok());
    assert_eq!(parse_csr(&[]), Err(CsrError::Parse));
    for cut in 0..good.len() {
        let result = parse_csr(&good[..cut]);
        assert_eq!(result, Err(CsrError::Parse), "cut at {cut}");
    }
    let mut with_tail = good.clone();
    with_tail.push(0);
    assert_eq!(parse_csr(&with_tail), Err(CsrError::Trailing));

    let hand = |edit: &dyn Fn(&mut Hand)| {
        let mut hand = Hand::new(&key);
        edit(&mut hand);
        hand.signed(&key)
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("outer not a sequence", hand(&|h| h.outer_tag = 0x31)),
        ("request info not a sequence", hand(&|h| h.cri_tag = 0x31)),
        ("version not an integer", hand(&|h| h.version_tag = 0x04)),
        (
            "subject not a sequence",
            hand(&|h| h.subject = tlv(0x31, &[])),
        ),
        ("attributes with another tag", hand(&|h| h.attrs_tag = 0xa1)),
        ("no attributes", hand(&|h| h.no_attrs = true)),
        (
            "an attribute that is not a sequence",
            hand(&|h| h.attrs = vec![tlv(0x31, &[])]),
        ),
        (
            "an attribute without a set",
            hand(&|h| h.attrs = vec![seq(&tlv(0x06, &[0x55, 0x04, 0x03]))]),
        ),
        (
            "an attribute with a type that is not an OID",
            hand(&|h| h.attrs = vec![seq(&cat(&[&tlv(0x04, &[1]), &tlv(0x31, &[])]))]),
        ),
        (
            "an attribute with something after its set",
            hand(&|h| {
                h.attrs = vec![seq(&cat(&[
                    &tlv(0x06, &[0x55, 0x04, 0x03]),
                    &tlv(0x31, &[]),
                    &tlv(0x05, &[]),
                ]))]
            }),
        ),
        (
            "something after the attributes",
            hand(&|h| h.extra_in_cri = vec![0x05, 0x00]),
        ),
        ("algorithm not a sequence", hand(&|h| h.sig_alg_tag = 0x31)),
        ("signature not a bit string", hand(&|h| h.sig_tag = 0x04)),
        ("signature with unused bits", hand(&|h| h.sig_unused = 1)),
        (
            "something after the signature",
            hand(&|h| h.after_sig = vec![0x05, 0x00]),
        ),
    ];
    for (what, der) in cases {
        assert_eq!(parse_csr(&der), Err(CsrError::Parse), "{what}");
    }

    // A signature element with no bytes at all, not even the unused-bits byte.
    let mut headless = Hand::new(&key);
    headless.sig_tag = 0x03;
    let cri = headless.cri();
    let bare = tlv(
        0x30,
        &cat(&[&cri, &tlv(0x30, &support::SHA256_ALG), &tlv(0x03, &[])]),
    );
    assert_eq!(
        parse_csr(&bare),
        Err(CsrError::Parse),
        "an empty bit string"
    );

    // Lengths the DER rules forbid, planted where the version is.
    for (what, replacement) in [
        (
            "a long form for a short length",
            vec![0x02, 0x81, 0x01, 0x00],
        ),
        (
            "a two-byte length with a zero high byte",
            vec![0x02, 0x82, 0x00, 0x01, 0x00],
        ),
        ("an indefinite length", vec![0x02, 0x80, 0x00]),
        (
            "a four-byte length",
            vec![0x02, 0x84, 0x00, 0x00, 0x00, 0x01, 0x00],
        ),
    ] {
        let mut hand = Hand::new(&key);
        hand.version_raw = Some(replacement);
        assert_eq!(
            parse_csr(&hand.signed(&key)),
            Err(CsrError::Parse),
            "{what}"
        );
    }
}

// Catches: a size limit that is missing, off by one in either direction, or applied after
// the parse (a 64 KiB request must not cost a parse).
#[test]
fn the_limit_is_two_kib_and_exact() {
    assert_eq!(MAX_CSR_LEN, 2048);
    let key = key();
    // A signature is 70 to 72 bytes whatever it signs, so a request of a given size is
    // found by building again with the padding corrected until the size is hit.
    let exactly = |size: usize| {
        let mut pad = 300usize;
        for _ in 0..500 {
            let mut hand = Hand::new(&key);
            hand.subject = subject(&"x".repeat(pad));
            let der = hand.signed(&key);
            if der.len() == size {
                return der;
            }
            pad = (pad + size).saturating_sub(der.len());
        }
        panic!("no request of {size} bytes");
    };
    let at_limit = exactly(MAX_CSR_LEN);
    assert_eq!(at_limit.len(), MAX_CSR_LEN);
    assert!(parse_csr(&at_limit).is_ok(), "exactly 2 KiB is accepted");
    let over = exactly(MAX_CSR_LEN + 1);
    assert_eq!(over.len(), MAX_CSR_LEN + 1);
    assert_eq!(parse_csr(&over), Err(CsrError::TooLarge));
    assert_eq!(parse_csr(&vec![0u8; 65_536]), Err(CsrError::TooLarge));
}

// Catches: the OID constant drifting from the encoding the node's builder writes (the test
// builder here encodes it from the arcs, independently of the parser).
#[test]
fn issued_at_oid_is_in_the_uuid_arc() {
    assert_eq!(kbf_ca::ISSUED_AT_OID[..2], [2, 25]);
    assert_eq!(issued_at_oid_content().len(), 10);
}
