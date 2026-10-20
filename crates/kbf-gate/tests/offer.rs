//! `offer.json`'s contract: golden bytes, a reader that refuses what a deploy must not
//! trust, and the reversibility rule.

use kbf_gate::{
    Beta, CommitId, DocError, OFFER_VERSION, OfferDoc, SetHash, Target, Waiver, reversible,
};

const GOLDEN: &str = "tests/golden/offer.json";

fn doc() -> OfferDoc {
    OfferDoc {
        version: OFFER_VERSION,
        created: 1_000,
        target: Target::Gamma,
        commit: CommitId::parse(&"ab".repeat(20)).unwrap(),
        set_hash: SetHash::of_sums(b"deadbeef  kbf-linux-x86_64.tar.gz\n"),
        platforms: vec!["darwin-arm64".to_owned(), "linux-x86_64".to_owned()],
        beta: Beta::Attested {
            run_id: "beta-0001".to_owned(),
        },
        meta_log: Some(4),
        previous: Some(CommitId::parse(&"cd".repeat(20)).unwrap()),
        previous_meta_log: Some(4),
        reversible: true,
    }
}

fn waived() -> OfferDoc {
    OfferDoc {
        beta: Beta::Waived {
            waiver: Waiver::new("beta", "operator", "no full beta exists yet").unwrap(),
        },
        ..doc()
    }
}

fn offer_err(d: &OfferDoc) -> String {
    match d.validate() {
        Err(DocError::Offer(why)) => why,
        other => panic!("expected an offer error, got {other:?}"),
    }
}

#[test]
fn the_document_is_the_golden_bytes_and_reads_back_equal() {
    // Proves: the field names and shape U7 parses are pinned, and write then read is
    // the identity for both beta forms. Mutant: rename or drop a field.
    let bytes = doc().to_bytes().unwrap();
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        std::fs::read_to_string(GOLDEN).unwrap()
    );
    assert_eq!(OfferDoc::from_bytes(&bytes).unwrap(), doc());
    let w = waived().to_bytes().unwrap();
    assert_eq!(OfferDoc::from_bytes(&w).unwrap(), waived());
}

#[test]
fn reversible_needs_both_numbers_and_the_new_log_not_above_the_old() {
    // Proves D9 exactly: N <= N-1 reversible; N > N-1 not; unknown either side not.
    // Mutants: `<=` to `<` (equal row), `<=` to `>=` (greater row), unknown as 0.
    assert!(reversible(Some(4), Some(4)), "equal");
    assert!(reversible(Some(3), Some(4)), "lower");
    assert!(!reversible(Some(5), Some(4)), "raised");
    assert!(!reversible(None, Some(4)), "new unknown");
    assert!(!reversible(Some(4), None), "old unknown");
    assert!(!reversible(None, None), "both unknown");
    assert!(
        !reversible(Some(0), None),
        "zero is not unknown, but the old side is"
    );
}

#[test]
fn a_reader_refuses_unknown_fields_other_versions_and_edited_reversibility() {
    // Proves each refusal a deploy relies on. Mutants: drop deny_unknown_fields; drop
    // the version check; drop the reversible-consistency check.
    let text = String::from_utf8(doc().to_bytes().unwrap()).unwrap();
    let extra = text.replacen('{', "{\n  \"extra\": 1,", 1);
    assert!(matches!(
        OfferDoc::from_bytes(extra.as_bytes()),
        Err(DocError::Json(_))
    ));
    let other = text.replace("\"version\": 1", "\"version\": 2");
    assert!(offer_err_of(&other).contains("version 2"));
    // meta_log raised past the previous, `reversible` left true by hand.
    let forged = text.replace("\"meta_log\": 4,", "\"meta_log\": 5,");
    assert!(offer_err_of(&forged).contains("reversible"));
    // `reversible` flipped to true over an unknown number.
    let mut unknown = doc();
    unknown.previous_meta_log = None;
    assert!(offer_err(&unknown).contains("reversible"));
    unknown.reversible = false;
    assert!(unknown.validate().is_ok());
}

fn offer_err_of(text: &str) -> String {
    match OfferDoc::from_bytes(text.as_bytes()) {
        Err(DocError::Offer(why)) => why,
        other => panic!("expected an offer error, got {other:?}"),
    }
}

#[test]
fn a_prod_offer_cannot_carry_a_waiver_and_a_waiver_needs_its_reason() {
    // Mutants: drop the prod check; let Waiver::new pass an empty reason.
    let mut prod = waived();
    prod.target = Target::Prod;
    assert!(offer_err(&prod).contains("prod"));
    assert!(
        prod.to_bytes().is_err(),
        "a refused document is not written"
    );
    let mut blank = waived();
    blank.beta = Beta::Waived {
        waiver: Waiver {
            what: "beta".to_owned(),
            by: "operator".to_owned(),
            reason: " ".to_owned(),
        },
    };
    assert!(matches!(blank.validate(), Err(DocError::Waiver(_))));
    let mut none = doc();
    none.platforms.clear();
    assert!(offer_err(&none).contains("platform"));
}

#[test]
fn plain_serde_refuses_what_from_bytes_refuses() {
    // The type cannot be read around its validation: a prod waiver with a blank reason,
    // no platform and a forged `reversible`, through serde_json::from_slice directly.
    // Mutant: drop `try_from = "RawOfferDoc"`.
    let text = String::from_utf8(doc().to_bytes().unwrap()).unwrap();
    let forged = text
        .replace("\"gamma\"", "\"prod\"")
        .replace(
            "\"kind\": \"attested\",\n    \"run_id\": \"beta-0001\"",
            "\"kind\": \"waived\", \"waiver\": {\"what\":\"beta\",\"by\":\"\",\"reason\":\"\"}",
        )
        .replace("\"darwin-arm64\",\n    \"linux-x86_64\"", "")
        .replace(
            "\"previous\": \"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd\"",
            "\"previous\": null",
        );
    assert_ne!(forged, text);
    assert!(serde_json::from_slice::<OfferDoc>(forged.as_bytes()).is_err());
    assert!(OfferDoc::from_bytes(forged.as_bytes()).is_err());
    // Each single edit is refused by both readers, and the good one by both accepted.
    for edit in [
        text.replace(
            "\"previous\": \"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd\"",
            "\"previous\": null",
        ),
        text.replace("\"run_id\": \"beta-0001\"", "\"run_id\": \" \""),
        text.replace("\"gamma\"", "\"prod\"")
            .replace("\"attested\"", "\"waived\""),
    ] {
        assert_ne!(edit, text);
        assert!(
            serde_json::from_slice::<OfferDoc>(edit.as_bytes()).is_err(),
            "{edit}"
        );
        assert!(OfferDoc::from_bytes(edit.as_bytes()).is_err(), "{edit}");
    }
    assert_eq!(
        serde_json::from_slice::<OfferDoc>(text.as_bytes()).unwrap(),
        doc()
    );
}

#[test]
fn a_previous_meta_log_needs_a_previous_commit_and_an_attested_beta_needs_a_run() {
    // Mutants: drop each check in `validate`.
    let mut d = doc();
    d.previous = None;
    d.reversible = false;
    assert!(offer_err(&d).contains("previous meta_log without a previous commit"));
    d.previous_meta_log = None;
    assert!(d.validate().is_ok());
    let mut blank = doc();
    blank.beta = Beta::Attested {
        run_id: "  ".to_owned(),
    };
    assert!(offer_err(&blank).contains("names no run"));
}
