use super::*;
use crate::fixtures::COMMIT;

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn offer(bundle: &str, sha: &str, components: &str) -> String {
    format!(
        "{{\"schema\":1,\"commit\":\"{COMMIT}\",\"bundle\":{{\"file\":\"{bundle}\",\
         \"sha256\":\"{sha}\"}},\"components\":[{components}]}}"
    )
}

fn component(name: &str, file: &str, sha: &str) -> String {
    format!("{{\"name\":\"{name}\",\"file\":\"{file}\",\"sha256\":\"{sha}\"}}")
}

fn code(text: &str) -> &'static str {
    Offer::parse(text.as_bytes()).expect_err(text).code()
}

/// Catches an offer read wrongly: the commit, the bundle, each component with its known
/// name (as the `&'static str` the layout uses), and the set of file names the entry may
/// hold (the offer itself, the bundle and each tarball).
#[test]
fn a_good_offer_is_read_whole() {
    let both = [
        component("kbf", "kbf.tar.gz", SHA_A),
        component("kbf-daemon", "kbf-daemon.tar.gz", SHA_B),
    ]
    .join(",");
    let o = Offer::parse(offer("provenance.jsonl", SHA_B, &both).as_bytes()).expect("valid");
    assert_eq!(o.commit.as_str(), COMMIT);
    assert_eq!(
        o.bundle,
        OfferedFile {
            file: "provenance.jsonl".into(),
            sha256: SHA_B.into()
        }
    );
    let names: Vec<&str> = o.components.iter().map(|c| c.name).collect();
    assert_eq!(names, ["kbf", "kbf-daemon"]);
    assert_eq!(o.components[1].tarball.file, "kbf-daemon.tar.gz");
    let want: Vec<&str> = vec![
        "kbf-daemon.tar.gz",
        "kbf.tar.gz",
        "offer.json",
        "provenance.jsonl",
    ];
    assert_eq!(
        o.names().iter().map(String::as_str).collect::<Vec<_>>(),
        want
    );
}

/// Catches an offer accepted that is not exactly what a frontend writes: not JSON, an
/// unknown or missing field, another schema, a bad commit, no components, a component
/// that is not a binary of a release, one named twice, a file name that is a path, a
/// dotfile, `offer.json` itself, too long or has a space, one file named twice, and a
/// sha256 that is short, upper case or not hex.
///
/// Mutants seen red: the file-name check dropped (a `../x` name passes); the repeated
/// file check dropped.
#[test]
fn an_offer_that_is_not_exactly_the_shape_is_refused() {
    let kbf = component("kbf", "kbf.tar.gz", SHA_A);
    let ok = |c: &str| offer("provenance.jsonl", SHA_B, c);
    assert_eq!(code("not json"), "offer-malformed");
    assert_eq!(code("{}"), "offer-malformed");
    assert_eq!(
        code(&ok(&kbf).replace("\"schema\":1", "\"schema\":2")),
        "offer-schema"
    );
    assert_eq!(
        code(&ok(&kbf).replace("{\"schema\"", "{\"extra\":1,\"schema\"")),
        "offer-malformed"
    );
    assert_eq!(code(&ok(&kbf).replace(COMMIT, "HEAD")), "offer-commit");
    assert_eq!(
        code(&ok(&kbf).replace(COMMIT, &COMMIT.to_uppercase())),
        "offer-commit"
    );
    assert_eq!(code(&ok("")), "offer-no-components");
    assert_eq!(
        code(&ok(&component("kbf-deploy", "d.tar.gz", SHA_A))),
        "offer-component"
    );
    let twice = [kbf.clone(), component("kbf", "other.tar.gz", SHA_A)].join(",");
    assert_eq!(code(&ok(&twice)), "offer-repeated-component");
    for bad in [
        "../x.tar.gz",
        "a/b.tar.gz",
        "/etc/passwd",
        ".hidden",
        "",
        "has space.tar.gz",
        &"x".repeat(129),
        "tab\\there",
    ] {
        assert_eq!(
            code(&ok(&component("kbf", bad, SHA_A))),
            "offer-file-name",
            "{bad:?}"
        );
    }
    // `offer.json` is the offer, never a tarball or the bundle.
    assert_eq!(
        code(&ok(&component("kbf", "offer.json", SHA_A))),
        "offer-repeated-file"
    );
    assert_eq!(
        code(&offer("offer.json", SHA_B, &kbf)),
        "offer-repeated-file"
    );
    assert_eq!(
        code(&ok(&component("kbf", "provenance.jsonl", SHA_A))),
        "offer-repeated-file"
    );
    let same = [kbf.clone(), component("kbf-daemon", "kbf.tar.gz", SHA_B)].join(",");
    assert_eq!(code(&ok(&same)), "offer-repeated-file");
    for bad in [
        &SHA_A[..63],
        &SHA_A.to_uppercase(),
        &SHA_A.replace('a', "g"),
        "",
    ] {
        assert_eq!(
            code(&ok(&component("kbf", "kbf.tar.gz", bad))),
            "offer-sha256",
            "{bad:?}"
        );
        assert_eq!(
            code(&offer("provenance.jsonl", bad, &kbf)),
            "offer-sha256",
            "{bad:?}"
        );
    }
}

/// Catches the refusal messages an operator reads being wrong or empty.
#[test]
fn every_offer_refusal_has_a_one_line_message() {
    let faults = [
        OfferFault::NotAnOffer("eof".into()),
        OfferFault::Schema { found: 2, want: 1 },
        OfferFault::NoComponents,
        OfferFault::UnknownComponent("x".into()),
        OfferFault::RepeatedComponent("kbf".into()),
        OfferFault::BadFileName("../x".into()),
        OfferFault::RepeatedFile("a".into()),
        OfferFault::BadSha256("zz".into()),
        OfferFault::BadCommit("HEAD".into()),
    ];
    for f in faults {
        let text = f.to_string();
        assert!(!text.is_empty() && !text.contains('\n'), "{text}");
    }
}
