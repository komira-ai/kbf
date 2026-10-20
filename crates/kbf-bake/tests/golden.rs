//! The verdict and the tag body, as bytes. `golden/gamma-verdict.json` is the file a fixed
//! bake writes; it is checked in so that a change to the serializer, to the gate names or to
//! their order shows up as a diff, not as two sides that moved together.
//!
//! The tag message is the verdict text verbatim (docs/design/kbf-cicd.md, section 8.1), and
//! the other side of the contract is the release verify step of `cargo xtask release
//! verdict`. That parser (PR2 of this series) had not been pushed when this was written, so
//! the tag body is read back here with the shared type of `kbf-gate` (`ResultDoc`), which
//! PR2 is to read with too. When PR2 lands its parser takes over the read side of this test.
//!
//! `BLESS=1 cargo test -p kbf-bake --test golden` rewrites the golden file.

mod common;

use std::path::Path;

use common::*;
use kbf_bake::cli::{EXIT_GREEN, EXIT_USAGE, execute};
use kbf_bake::eval::Evaluator;
use kbf_gate::{ResultDoc, Stage, Status, Window};

const GOLDEN: &str = "tests/golden/gamma-verdict.json";

fn golden_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN)
}

/// A four hour bake of a farm with load, generation 7, one earlier deploy.
fn fixed_bake() -> Vec<u8> {
    let bake = Bake::new("golden");
    let mut s = state(4 * 3600);
    s.desired_generation = Some(7);
    s.deploy = Some(Window {
        start: T0 - 900,
        end: Some(T0 - 60),
    });
    let mut e = Evaluator::new(s, 3 * INTERVAL);
    let out = bake.drive(&mut e, growing(10.0), Some(12), Some(quiet_log()));
    assert_eq!(out.status, Status::Green, "{}", out.note);
    std::fs::read(&bake.verdict).unwrap()
}

/// Catches a verdict whose bytes drifted: the evaluator's output for the fixed bake is the
/// checked-in file, byte for byte.
#[test]
fn the_verdict_of_a_fixed_bake_is_the_golden_file() {
    let bytes = fixed_bake();
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(golden_path(), &bytes).unwrap();
    }
    let golden = std::fs::read(golden_path()).unwrap();
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        String::from_utf8(golden).unwrap()
    );
}

/// Catches a tag body that is not the verdict verbatim, and one that the shared reader
/// refuses or reads as something else: the body is the golden bytes, and reads back as a
/// finished green gamma verdict naming the bake's commit, set and generation.
#[test]
fn the_tag_message_is_the_verdict_verbatim_and_reads_back() {
    let args = [
        "kbf-bake",
        "tag-message",
        "--verdict",
        golden_path().to_str().unwrap(),
    ]
    .map(Into::into)
    .to_vec();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(execute(args, &mut out, &mut err), EXIT_GREEN);
    assert!(err.is_empty());
    assert_eq!(out, std::fs::read(golden_path()).unwrap());
    let doc = ResultDoc::from_bytes(&out).unwrap();
    assert_eq!((doc.stage, doc.status), (Stage::Gamma, Status::Green));
    assert_eq!(doc.commit, commit());
    assert_eq!(doc.set_hash, set_hash());
    assert_eq!(doc.desired_generation, Some(7));
    assert_eq!(doc.gates.len(), 11);
}

/// Catches a tag body made from a verdict that must not be tagged: a running, red or extended
/// one, another stage's, and one that is not a verdict.
#[test]
fn only_a_finished_green_gamma_verdict_makes_a_tag_message() {
    let golden = std::fs::read_to_string(golden_path()).unwrap();
    let dir = scratch("golden-refusals");
    for (name, text) in [
        (
            "running",
            golden.replace("\"status\": \"green\"", "\"status\": \"running\""),
        ),
        (
            "red",
            golden.replace("\"status\": \"green\"", "\"status\": \"red\""),
        ),
        (
            "extended",
            golden.replace("\"status\": \"green\"", "\"status\": \"extended\""),
        ),
        (
            "beta",
            golden.replace("\"stage\": \"gamma\"", "\"stage\": \"beta\""),
        ),
        ("junk", "{}".to_owned()),
    ] {
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, text).unwrap();
        let args = [
            "kbf-bake",
            "tag-message",
            "--verdict",
            path.to_str().unwrap(),
        ]
        .map(Into::into)
        .to_vec();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(execute(args, &mut out, &mut err), EXIT_USAGE, "{name}");
        assert!(out.is_empty(), "{name}: nothing is printed");
    }
    let args = [
        "kbf-bake",
        "tag-message",
        "--verdict",
        "/no/such/verdict.json",
    ]
    .map(Into::into)
    .to_vec();
    assert_eq!(execute(args, &mut Vec::new(), &mut Vec::new()), EXIT_USAGE);
}
