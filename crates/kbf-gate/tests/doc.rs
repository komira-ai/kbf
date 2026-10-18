//! The document's contract: golden bytes, the fields later stages need, fail-closed reads.

use kbf_gate::{
    CommitId, DocError, Floor, GateResult, Header, Limit, MinLoad, ResultDoc, Sample, SetHash,
    Stage, Status, Verdict, Waiver, Window,
};

const GOLDEN: &str = "tests/golden/result_doc.json";

fn header() -> Header {
    Header {
        stage: Stage::Gamma,
        commit: CommitId::parse(&"ab".repeat(20)).unwrap(),
        set_hash: SetHash::of_sums(b"deadbeef  kbf-linux-x86_64.tar.gz\n"),
        run_id: "bake-0001".to_owned(),
        attempt: 1,
    }
}

/// A document with every field set, so the golden file pins them all.
fn full() -> ResultDoc {
    let mut d = ResultDoc::begin(header(), 1_000);
    d.status = Status::Green;
    d.finished = Some(90_000);
    d.desired_generation = Some(7);
    d.deploy = Some(Window {
        start: 1_100,
        end: Some(1_900),
    });
    d.bake = Some(Window {
        start: 2_000,
        end: Some(88_400),
    });
    let floor = |measured, required| Floor { measured, required };
    d.load = Some(MinLoad {
        actions: floor(1_000, 1_000),
        builds: floor(12, 10),
        hours: floor(3, 3),
    });
    d.waivers = vec![Waiver::new("min-load", "operator", "holiday week").unwrap()];
    d.gates = vec![
        GateResult::decide(
            "lost-blobs",
            Sample::Value(0.0),
            Limit::AtMost(0.0),
            2_000,
            88_400,
        ),
        GateResult::decide(
            "latency-p99",
            Sample::Value(1.8),
            Limit::Band {
                warn: 1.5,
                red: 3.0,
            },
            2_000,
            88_400,
        ),
        GateResult::decide(
            "tests",
            Sample::Value(3.0),
            Limit::AtLeast(1.0),
            2_000,
            88_400,
        ),
    ];
    d
}

/// Catches a serializer change (a renamed or reordered field, another number format): the
/// bytes of a full document must equal the checked-in file. `BLESS=1` rewrites it, the way
/// kbf-cli's goldens do; the diff is then reviewed.
#[test]
fn the_document_bytes_match_the_golden_file() {
    let bytes = full().to_bytes().unwrap();
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(GOLDEN, &bytes).unwrap();
    }
    let golden = std::fs::read(GOLDEN).expect("golden file");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        String::from_utf8_lossy(&golden)
    );
    assert_eq!(ResultDoc::from_bytes(&golden).unwrap(), full());
}

#[test]
fn the_fields_the_later_stages_need_are_in_the_document() {
    let text = String::from_utf8(full().to_bytes().unwrap()).unwrap();
    for key in [
        "\"desired_generation\"",
        "\"deploy\"",
        "\"bake\"",
        "\"load\"",
        "\"waivers\"",
        "\"set_hash\"",
        "\"run_id\"",
        "\"attempt\"",
    ] {
        assert!(text.contains(key), "{key} is missing:\n{text}");
    }
}

#[test]
fn a_green_document_with_a_red_gate_is_refused() {
    let mut d = full();
    d.gates[0] = GateResult::decide("lost-blobs", Sample::Absent, Limit::AtMost(0.0), 1, 2);
    let bytes = d.to_bytes().unwrap();
    assert_eq!(
        ResultDoc::from_bytes(&bytes).unwrap_err(),
        DocError::GreenButNot("a gate is red")
    );
}

#[test]
fn a_green_document_with_no_gate_or_no_finish_is_refused() {
    let mut d = full();
    d.gates.clear();
    assert!(matches!(
        ResultDoc::from_bytes(&d.to_bytes().unwrap()),
        Err(DocError::GreenButNot("it has no gate"))
    ));
    let mut d = full();
    d.finished = None;
    assert!(matches!(
        ResultDoc::from_bytes(&d.to_bytes().unwrap()),
        Err(DocError::GreenButNot("it has no finish time"))
    ));
}

#[test]
fn an_unknown_schema_field_or_garbage_is_refused() {
    let mut d = full();
    d.schema = 2;
    assert_eq!(
        ResultDoc::from_bytes(&d.to_bytes().unwrap()).unwrap_err(),
        DocError::Schema { found: 2 }
    );
    let text = String::from_utf8(full().to_bytes().unwrap()).unwrap();
    let extra = text.replacen("{\n", "{\n  \"surprise\": 1,\n", 1);
    assert!(matches!(
        ResultDoc::from_bytes(extra.as_bytes()),
        Err(DocError::Malformed(_))
    ));
    assert!(matches!(
        ResultDoc::from_bytes(b""),
        Err(DocError::Malformed(_))
    ));
}

#[test]
fn a_waiver_needs_an_operator_and_a_reason_also_when_read() {
    assert!(Waiver::new("min-load", "", "because").is_err());
    assert!(Waiver::new("min-load", "operator", "  ").is_err());
    let mut d = full();
    d.waivers[0].reason = String::new();
    assert!(matches!(
        ResultDoc::from_bytes(&d.to_bytes().unwrap()),
        Err(DocError::Waiver(_))
    ));
}

/// Equal passes, one below fails, for each of the three floors on its own.
#[test]
fn the_load_floors_are_exact() {
    let floor = |measured, required| Floor { measured, required };
    let at = |a, b, h| MinLoad {
        actions: floor(a, 1000),
        builds: floor(b, 10),
        hours: floor(h, 3),
    };
    assert!(at(1000, 10, 3).met());
    assert!(!at(999, 10, 3).met());
    assert!(!at(1000, 9, 3).met());
    assert!(!at(1000, 10, 2).met());
}

#[test]
fn hashes_are_lower_case_hex_of_the_right_length() {
    assert!(CommitId::parse(&"a".repeat(40)).is_ok());
    assert!(CommitId::parse(&"a".repeat(64)).is_ok());
    assert!(CommitId::parse(&"a".repeat(41)).is_err());
    assert!(CommitId::parse(&"A".repeat(40)).is_err());
    assert!(SetHash::parse(&"a".repeat(40)).is_err());
    assert_eq!(
        CommitId::parse(&"c".repeat(40)).unwrap().as_str(),
        "c".repeat(40)
    );
    let json = serde_json::to_string(&SetHash::parse(&"b".repeat(64)).unwrap()).unwrap();
    assert_eq!(json, format!("\"{}\"", "b".repeat(64)));
}

/// The one definition of the set hash: SHA-256 of the SHA256SUMS bytes, nothing else
/// (not normalised, not sorted). The vector is SHA-256("abc").
#[test]
fn the_set_hash_is_the_sha256_of_the_sums_bytes() {
    assert_eq!(
        SetHash::of_sums(b"abc").as_str(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_ne!(SetHash::of_sums(b"abc\n"), SetHash::of_sums(b"abc"));
}

#[test]
fn the_status_is_green_only_with_gates_and_none_red() {
    let mut d = ResultDoc::begin(header(), 1);
    assert_eq!(
        d.status_from_gates(),
        Status::Red,
        "no gate decided nothing"
    );
    let g = |v| GateResult {
        verdict: v,
        ..GateResult::decide("g", Sample::Value(0.0), Limit::AtMost(0.0), 1, 1)
    };
    d.gates = vec![g(Verdict::Green), g(Verdict::Warn)];
    assert_eq!(d.status_from_gates(), Status::Green);
    d.gates.push(g(Verdict::Red));
    assert_eq!(d.status_from_gates(), Status::Red);
    assert!(!d.passed());
    d.status = Status::Green;
    assert!(d.passed());
    for s in [Status::Running, Status::Red, Status::Extended] {
        d.status = s;
        assert!(!d.passed(), "{s:?} is not a pass");
    }
}
