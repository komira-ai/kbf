//! The contract types: steps and their arguments, exit codes, the JSON last line, the params
//! file, and the table of what the updater runs next.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use update_hook::{
    ArgsError, Class, Context, Invocation, LineError, Next, Outcome, PROMOTION_CEILING_KEY, Params,
    ParamsError, Phase, Report, Scalar, Step, Verdict, next,
};

mod support;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

// --- steps and arguments ---

/// Catches a step whose word on the command line is not the contract's.
#[test]
fn the_five_steps_have_the_words_of_the_contract() {
    let words: Vec<&str> = Step::ALL.iter().map(|s| s.name()).collect();
    assert_eq!(
        words,
        ["prepare", "cutover", "verify", "rollback", "cleanup"]
    );
    for step in Step::ALL {
        assert_eq!(step.to_string(), step.name());
        assert_eq!(step.name().parse::<Step>(), Ok(step));
    }
    assert_eq!(
        "restore".parse::<Step>(),
        Err(ArgsError::UnknownStep("restore".to_owned()))
    );
}

/// Catches an argument parser that accepts what the updater never sends, or refuses what it
/// does: `prepare <artifact>`, the other four steps bare, `--params <file>` after any.
#[test]
fn a_command_line_parses_to_the_step_its_artifact_and_its_params_file() {
    let prepare =
        Invocation::parse(os(&["prepare", "/rel/abc", "--params", "/run/p.toml"])).unwrap();
    assert_eq!(prepare.step, Step::Prepare);
    assert_eq!(prepare.artifact, Some(PathBuf::from("/rel/abc")));
    assert_eq!(prepare.params, Some(PathBuf::from("/run/p.toml")));

    // The options may come before the artifact.
    let swapped =
        Invocation::parse(os(&["prepare", "--params", "/run/p.toml", "/rel/abc"])).unwrap();
    assert_eq!(swapped, prepare);

    for step in ["cutover", "verify", "rollback", "cleanup"] {
        let inv = Invocation::parse(os(&[step])).unwrap();
        assert_eq!(inv.step.name(), step);
        assert_eq!((inv.artifact, inv.params), (None, None));
    }
}

/// Catches `to_args` and `parse` drifting apart, which would make a hook and the updater
/// disagree on the command line.
#[test]
fn printing_a_command_line_and_parsing_it_back_is_the_identity() {
    let all = [
        Invocation {
            step: Step::Prepare,
            artifact: Some("/a".into()),
            params: Some("/p".into()),
        },
        Invocation {
            step: Step::Prepare,
            artifact: Some("/a".into()),
            params: None,
        },
        Invocation {
            step: Step::Cutover,
            artifact: None,
            params: Some("/p".into()),
        },
        Invocation {
            step: Step::Cleanup,
            artifact: None,
            params: None,
        },
    ];
    for inv in all {
        assert_eq!(Invocation::parse(inv.to_args()).unwrap(), inv);
    }
    assert_eq!(
        Invocation {
            step: Step::Verify,
            artifact: None,
            params: Some("/p".into())
        }
        .to_args(),
        os(&["verify", "--params", "/p"])
    );
}

/// Catches each way a command line can be wrong going unreported.
#[test]
fn a_wrong_command_line_names_what_is_wrong() {
    let cases: [(&[&str], ArgsError); 8] = [
        (&[], ArgsError::MissingStep),
        (&["restore"], ArgsError::UnknownStep("restore".to_owned())),
        (&["prepare"], ArgsError::MissingArtifact),
        (&["prepare", "--params", "/p"], ArgsError::MissingArtifact),
        (
            &["prepare", "/a", "/b"],
            ArgsError::Unexpected(OsString::from("/b")),
        ),
        (
            &["cutover", "/a"],
            ArgsError::Unexpected(OsString::from("/a")),
        ),
        (&["verify", "--params"], ArgsError::MissingParamsFile),
        (
            &["verify", "--params", "/a", "--params", "/b"],
            ArgsError::DuplicateParams,
        ),
    ];
    for (args, want) in cases {
        assert_eq!(Invocation::parse(os(args)), Err(want), "{args:?}");
    }
    for error in [
        ArgsError::MissingStep,
        ArgsError::UnknownStep("x".to_owned()),
        ArgsError::MissingArtifact,
        ArgsError::Unexpected(OsString::from("x")),
        ArgsError::MissingParamsFile,
        ArgsError::DuplicateParams,
    ] {
        assert!(!error.to_string().is_empty());
    }
}

// --- exit codes ---

/// Catches an exit code mapped to the wrong outcome: 0 ok, 1 failure, 75 deferred, 76 halt, and
/// any other code or no code (a signal, a timeout) a failure.
#[test]
fn exit_codes_map_to_outcomes() {
    let table = [
        (0, Outcome::Success),
        (1, Outcome::Failure),
        (75, Outcome::Deferred),
        (76, Outcome::Halt),
    ];
    for (code, outcome) in table {
        assert_eq!(Outcome::from_code(code), Some(outcome));
        assert_eq!(Outcome::from_exit(Some(code)), outcome);
        assert_eq!(outcome.code(), code);
    }
    for other in [2, 74, 77, 101, 127, 137, -1] {
        assert_eq!(Outcome::from_code(other), None);
        assert_eq!(Outcome::from_exit(Some(other)), Outcome::Failure);
    }
    assert_eq!(Outcome::from_exit(None), Outcome::Failure);
    assert_eq!(
        (
            update_hook::EXIT_OK,
            update_hook::EXIT_FAILURE,
            update_hook::EXIT_DEFERRED,
            update_hook::EXIT_HALT
        ),
        (0, 1, 75, 76)
    );
}

/// Catches a hook's `main` exiting with a code other than its outcome's.
#[test]
fn an_outcome_is_an_exit_code_for_main() {
    for (code, outcome) in [
        (0u8, Outcome::Success),
        (1, Outcome::Failure),
        (75, Outcome::Deferred),
        (76, Outcome::Halt),
    ] {
        assert_eq!(
            format!("{:?}", ExitCode::from(outcome)),
            format!("{:?}", ExitCode::from(code))
        );
    }
}

// --- the JSON last line ---

/// Catches a default that is not `{}`: a step that dies without the line is a `release` failure
/// that can still be undone, with nothing pending.
#[test]
fn a_step_that_dies_without_the_line_is_class_release() {
    for stdout in [
        "",
        "\n\n",
        "no json here",
        "{\"class\":",
        "[1]",
        "{\"class\":\"sideways\"}",
    ] {
        let report = Report::read(stdout);
        assert_eq!(report, Report::default(), "{stdout:?}");
        assert_eq!(report.class, Class::Release);
        assert!(report.reversible && !report.promotion_pending);
    }
    assert_eq!(Report::from_stdout(""), Err(LineError::Missing));
    assert_eq!(Report::from_stdout("\n  \n"), Err(LineError::Missing));
    assert!(matches!(
        Report::from_stdout("{\"class\":"),
        Err(LineError::Malformed(_))
    ));
    assert!(matches!(
        Report::from_stdout("[1]"),
        Err(LineError::Malformed(_))
    ));
    assert!(matches!(
        Report::from_stdout("{\"class\":\"sideways\"}"),
        Err(LineError::Malformed(_))
    ));
    for error in [
        LineError::Missing,
        LineError::Malformed("x".to_owned()),
        LineError::BadReason("X".to_owned()),
    ] {
        assert!(!error.to_string().is_empty());
    }
}

/// Catches a parser that reads the wrong line (the first, or one other than the last) or drops
/// a field.
#[test]
fn the_last_line_is_read_with_all_its_fields() {
    let stdout = concat!(
        "{\"class\":\"environment\"}\n",
        "progress: 40%\n",
        "{\"class\":\"environment\",\"reason\":\"lease_unreachable\",\"retry_after_secs\":300,",
        "\"reversible\":false,\"promotion_pending\":true,\"detail\":\"no hello\",\"extra\":1}\n",
        "\n"
    );
    let report = Report::read(stdout);
    assert_eq!(
        report,
        Report {
            class: Class::Environment,
            reason: Some("lease_unreachable".to_owned()),
            retry_after_secs: Some(300),
            reversible: false,
            reversible_stated: false,
            promotion_pending: true,
            detail: Some("no hello".to_owned()),
        }
    );
    // A field left out takes its default.
    let partial = Report::from_stdout("{\"reversible\":false}").unwrap();
    assert_eq!(
        (partial.class, partial.reversible, partial.promotion_pending),
        (Class::Release, false, false)
    );
    assert_eq!(Class::Release.as_str(), "release");
    assert_eq!(Class::Environment.as_str(), "environment");
}

/// Catches a printed line the reader does not read back to the same report, or one that is not
/// a single line.
#[test]
fn a_report_prints_as_one_line_and_reads_back() {
    let full = Report {
        class: Class::Environment,
        reason: Some("r".to_owned()),
        retry_after_secs: Some(7),
        reversible: false,
        reversible_stated: false,
        promotion_pending: true,
        detail: Some("two\nlines".to_owned()),
    };
    for report in [Report::default(), full] {
        let line = report.to_line();
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(Report::parse_line(&line).unwrap(), report);
        let mut out = Vec::new();
        report.write_to(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{line}\n"));
        assert_eq!(Report::read(&format!("noise\n{line}\n")), report);
    }
    // Left-out optional fields are not printed as null.
    let line = Report::default().to_line();
    assert!(
        !line.contains("null") && !line.contains("reason") && !line.contains("detail"),
        "{line}"
    );
}

/// Catches a reason that is prose or unbounded: the coordinator matches on it.
#[test]
fn a_reason_is_a_short_lowercase_token() {
    for good in ["state_invalid", "drain_bound", "a", "x9_"] {
        assert!(update_hook::is_reason_token(good), "{good}");
        let line = format!("{{\"reason\":\"{good}\"}}");
        assert_eq!(
            Report::parse_line(&line).unwrap().reason.as_deref(),
            Some(good)
        );
    }
    let long = "a".repeat(update_hook::MAX_REASON_LEN + 1);
    assert!(update_hook::is_reason_token(&long[1..]));
    for bad in [
        "",
        "Drain",
        "drain bound",
        "drain-bound",
        "d\u{e9}j\u{e0}",
        long.as_str(),
    ] {
        assert!(!update_hook::is_reason_token(bad), "{bad}");
        let line = serde_json_line(bad);
        assert_eq!(
            Report::parse_line(&line),
            Err(LineError::BadReason(bad.to_owned()))
        );
        assert_eq!(Report::read(&line), Report::default());
    }
}

fn serde_json_line(reason: &str) -> String {
    Report {
        reason: Some(reason.to_owned()),
        ..Report::default()
    }
    .to_line()
}

/// Catches a lenient read that throws away good fields with a bad one: a valid object keeps every
/// valid field, an invalid field takes its default and is warned about, and a `reversible` that
/// is present but not a boolean reads as false (a step that might have committed is treated as
/// committed). Only a missing line, or one that is not an object, is `{}`.
#[test]
fn the_lenient_read_keeps_every_valid_field() {
    let read = |line: &str| Report::read_with_warnings(line);
    // a bad reason does not drop the commit
    let (report, warnings) =
        read("{\"reversible\":false,\"reason\":\"Bad Reason\",\"class\":\"environment\"}");
    assert_eq!(
        (report.reversible, report.reason, report.class),
        (false, None, Class::Environment)
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("reason"), "{warnings:?}");
    // an unknown class takes the default and keeps the rest
    let (report, warnings) =
        read("{\"class\":\"sideways\",\"reversible\":false,\"retry_after_secs\":9}");
    assert_eq!(
        (report.class, report.reversible, report.retry_after_secs),
        (Class::Release, false, Some(9))
    );
    assert_eq!(warnings.len(), 1);
    // reversible present and not a boolean: false
    for line in [
        "{\"reversible\":\"no\"}",
        "{\"reversible\":null}",
        "{\"reversible\":1}",
    ] {
        let (report, warnings) = read(line);
        assert!(!report.reversible, "{line}");
        assert_eq!(warnings.len(), 1, "{line}");
    }
    // every other wrongly typed field takes its default
    let (report, warnings) = read(
        "{\"retry_after_secs\":\"soon\",\"promotion_pending\":1,\"detail\":5,\"reason\":7,\"extra\":true}",
    );
    assert_eq!(report, Report::default());
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    // a valid line: no warnings
    let (report, warnings) = read(
        "noise\n{\"class\":\"environment\",\"retry_after_secs\":300,\"reason\":\"drain_bound\"}\n",
    );
    assert_eq!(
        (
            report.class,
            report.retry_after_secs,
            report.reason.as_deref()
        ),
        (Class::Environment, Some(300), Some("drain_bound"))
    );
    assert!(warnings.is_empty());
    // no line: {} without a warning; not an object: {} with one
    assert_eq!(read(""), (Report::default(), Vec::new()));
    for line in ["[1]", "nope", "{\"class\":"] {
        let (report, warnings) = read(line);
        assert_eq!(report, Report::default(), "{line}");
        assert_eq!(warnings.len(), 1, "{line}");
    }
    // the strict read stays strict
    assert!(matches!(
        Report::from_stdout("{\"reversible\":\"no\"}"),
        Err(LineError::Malformed(_))
    ));
    assert!(!Report::read("{\"reversible\":false,\"reason\":\"Bad\"}").reversible);
}

// --- the params table ---

/// Catches a reader that drops a value or reads a type wrong.
#[test]
fn a_params_file_is_a_flat_table_of_scalars() {
    let params = Params::parse(
        "promotion_bound_secs = 900\nverbose = true\nratio = 0.5\nname = \"x\"\nneg = -3\n",
    )
    .unwrap();
    assert_eq!(
        params.get("promotion_bound_secs"),
        Some(&Scalar::Integer(900))
    );
    assert_eq!(params.get("verbose"), Some(&Scalar::Bool(true)));
    assert_eq!(params.get("ratio"), Some(&Scalar::Float(0.5)));
    assert_eq!(params.get("name"), Some(&Scalar::Text("x".to_owned())));
    assert_eq!(params.get("absent"), None);
    assert_eq!(params.get_u64("promotion_bound_secs"), Some(900));
    assert_eq!(params.get_u64("neg"), None);
    assert_eq!(params.get_u64("verbose"), None);
    assert_eq!(params.get_bool("verbose"), Some(true));
    assert_eq!(params.get_bool("name"), None);
    assert_eq!(params.promotion_ceiling_secs(), None);
    assert_eq!(Params::parse("").unwrap(), Params::default());
}

/// Catches the reserved key being lost or misread: it is one flat key with a dot in it, so it
/// is quoted in the file, and the updater's ceiling reaches the hook through it.
#[test]
fn the_reserved_key_carries_the_promotion_ceiling() {
    assert_eq!(PROMOTION_CEILING_KEY, "updater.promotion_ceiling_secs");
    let params = Params::parse("\"updater.promotion_ceiling_secs\" = 3600\nother = 1\n").unwrap();
    assert_eq!(params.promotion_ceiling_secs(), Some(3600));
    let written = Params::default().with_promotion_ceiling_secs(900);
    assert_eq!(written.promotion_ceiling_secs(), Some(900));
    assert_eq!(Params::parse(&written.to_toml()).unwrap(), written);
    // Everything survives a write and a read.
    let mut all = Params::default();
    all.insert("a", Scalar::Bool(false));
    all.insert("b", Scalar::Integer(5));
    all.insert("c", Scalar::Float(1.5));
    all.insert("d", Scalar::Text("t".to_owned()));
    assert_eq!(Params::parse(&all.to_toml()).unwrap(), all);
    assert_eq!(Params::default().to_toml(), "");
}

/// Catches a reserved key lost on the way to the hook, or one that reads as present when the
/// updater left it out (a first install has no previous release or version).
#[test]
fn the_updater_tells_the_hook_which_releases_and_versions() {
    assert_eq!(update_hook::STAGED_RELEASE_KEY, "updater.staged_release");
    assert_eq!(
        update_hook::PREVIOUS_RELEASE_KEY,
        "updater.previous_release"
    );
    assert_eq!(update_hook::TARGET_VERSION_KEY, "updater.target_version");
    assert_eq!(
        update_hook::PREVIOUS_VERSION_KEY,
        "updater.previous_version"
    );
    let update = Params::parse(concat!(
        "\"updater.staged_release\" = \"/var/lib/updater/releases/s/abc\"\n",
        "\"updater.previous_release\" = \"/var/lib/updater/releases/s/def\"\n",
        "\"updater.target_version\" = \"1.4.0\"\n",
        "\"updater.previous_version\" = \"1.3.0\"\n",
    ))
    .unwrap();
    assert_eq!(
        update.staged_release(),
        Some(PathBuf::from("/var/lib/updater/releases/s/abc"))
    );
    assert_eq!(
        update.previous_release(),
        Some(PathBuf::from("/var/lib/updater/releases/s/def"))
    );
    assert_eq!(update.target_version(), Some("1.4.0"));
    assert_eq!(update.previous_version(), Some("1.3.0"));
    assert_eq!(Params::parse(&update.to_toml()).unwrap(), update);

    let first = Params::parse(
        "\"updater.staged_release\" = \"/r/abc\"\n\"updater.target_version\" = \"1.0.0\"\n",
    )
    .unwrap();
    assert_eq!(first.staged_release(), Some(PathBuf::from("/r/abc")));
    assert_eq!(
        (first.previous_release(), first.previous_version()),
        (None, None)
    );
    let none = Params::default();
    assert_eq!((none.staged_release(), none.target_version()), (None, None));
    // A reserved key that is not text is refused, not read as absent.
    assert!(matches!(
        Params::parse("\"updater.target_version\" = 3\n"),
        Err(ParamsError::BadReserved(k)) if k == "updater.target_version"
    ));
}

/// Catches a params file with one previous-release key and not the other: a first install has
/// neither, an update has both, and a hook's validator refuses the half-way states.
#[test]
fn the_previous_release_and_version_go_together() {
    let release = "\"updater.previous_release\" = \"/r/def\"\n";
    let version = "\"updater.previous_version\" = \"1.3.0\"\n";
    assert!(Params::parse(&format!("{release}{version}")).is_ok());
    assert!(Params::parse("").is_ok());
    assert!(matches!(
        Params::parse(release),
        Err(ParamsError::PreviousPair)
    ));
    assert!(matches!(
        Params::parse(version),
        Err(ParamsError::PreviousPair)
    ));
    // The updater checks the table before it writes the file.
    let mut params = Params::default();
    assert!(params.validate().is_ok());
    params.insert(
        update_hook::PREVIOUS_VERSION_KEY,
        Scalar::Text("1.3.0".to_owned()),
    );
    assert!(matches!(params.validate(), Err(ParamsError::PreviousPair)));
    params.insert(
        update_hook::PREVIOUS_RELEASE_KEY,
        Scalar::Text("/r/def".to_owned()),
    );
    assert!(params.validate().is_ok());
    assert!(!ParamsError::PreviousPair.to_string().is_empty());
}

/// Catches a staged or previous release that is not an absolute path, which a hook would resolve
/// against its own working directory.
#[test]
fn the_release_paths_must_be_absolute() {
    for key in ["updater.staged_release", "updater.previous_release"] {
        let with_version = if key == "updater.previous_release" {
            "\"updater.previous_version\" = \"1\"\n"
        } else {
            ""
        };
        let text = format!("\"{key}\" = \"relative/dir\"\n{with_version}");
        assert!(
            matches!(Params::parse(&text), Err(ParamsError::NotAbsolute(k)) if k == key),
            "{key}"
        );
    }
    let mut params = Params::default();
    params.insert(
        update_hook::STAGED_RELEASE_KEY,
        Scalar::Text("rel".to_owned()),
    );
    assert!(matches!(
        params.validate(),
        Err(ParamsError::NotAbsolute(_))
    ));
    params.insert(
        update_hook::STAGED_RELEASE_KEY,
        Scalar::Text("/abs".to_owned()),
    );
    assert!(params.validate().is_ok());
    assert!(
        !ParamsError::NotAbsolute("k".to_owned())
            .to_string()
            .is_empty()
    );
}

/// Catches a digest that is not `sha256:` and 64 lowercase hex digits: the hook keys its record
/// on it, so a different shape would be a different identity for the same artifact.
#[test]
fn the_artifact_digest_is_sha256_and_64_lowercase_hex() {
    assert_eq!(update_hook::ARTIFACT_DIGEST_KEY, "updater.artifact_digest");
    let good = format!("sha256:{}", "0123456789abcdef".repeat(4));
    let params = Params::parse(&format!("\"updater.artifact_digest\" = \"{good}\"\n")).unwrap();
    assert_eq!(params.artifact_digest().unwrap(), Some(good.as_str()));
    assert_eq!(Params::default().artifact_digest().unwrap(), None);
    let short = format!("sha256:{}", "a".repeat(63));
    let long = format!("sha256:{}", "a".repeat(65));
    let upper = format!("sha256:{}", "A".repeat(64));
    let other = format!("sha512:{}", "a".repeat(64));
    let bare = "a".repeat(64);
    for bad in [short, long, upper, other, bare, String::new()] {
        let text = format!("\"updater.artifact_digest\" = \"{bad}\"\n");
        assert!(
            matches!(Params::parse(&text), Err(ParamsError::BadDigest(d)) if d == bad),
            "{bad}"
        );
        let mut params = Params::default();
        params.insert(update_hook::ARTIFACT_DIGEST_KEY, Scalar::Text(bad.clone()));
        assert!(
            matches!(params.artifact_digest(), Err(ParamsError::BadDigest(_))),
            "{bad}"
        );
    }
    assert!(matches!(
        Params::parse("\"updater.artifact_digest\" = 5\n"),
        Err(ParamsError::BadReserved(_))
    ));
    assert!(matches!(
        Params::parse_declared(&format!("\"updater.artifact_digest\" = \"{good}\"\n")),
        Err(ParamsError::ReservedKey(_))
    ));
    assert!(
        !ParamsError::BadDigest("x".to_owned())
            .to_string()
            .is_empty()
    );
}

/// Catches a declaration that sets a key only the updater may write: its own `[hook.params]`
/// may carry anything but the `updater.` namespace.
#[test]
fn a_declaration_may_not_set_an_updater_key() {
    assert_eq!(
        Params::parse_declared("promotion_bound_secs = 900\n")
            .unwrap()
            .get_u64("promotion_bound_secs"),
        Some(900)
    );
    for text in [
        "\"updater.staged_release\" = \"/etc\"\n",
        "\"updater.promotion_ceiling_secs\" = 1\n",
        "\"updater.anything_new\" = true\n",
    ] {
        assert!(
            matches!(
                Params::parse_declared(text),
                Err(ParamsError::ReservedKey(_))
            ),
            "{text}"
        );
    }
    // Other errors still come through.
    assert!(matches!(
        Params::parse_declared("list = [1]\n"),
        Err(ParamsError::NotScalar(_))
    ));
    for error in [
        ParamsError::BadReserved("k".to_owned()),
        ParamsError::ReservedKey("k".to_owned()),
    ] {
        assert!(!error.to_string().is_empty());
    }
}

/// Catches a table the updater would refuse being accepted by a hook, or the reverse.
#[test]
fn a_params_file_that_is_not_a_table_of_scalars_is_refused() {
    for (text, key) in [
        ("list = [1, 2]\n", "list"),
        ("[nested]\na = 1\n", "nested"),
        ("updater.promotion_ceiling_secs = 3600\n", "updater"),
        ("when = 2026-10-01\n", "when"),
    ] {
        match Params::parse(text) {
            Err(ParamsError::NotScalar(k)) => assert_eq!(k, key, "{text}"),
            other => panic!("{text:?}: {other:?}"),
        }
    }
    assert!(matches!(
        Params::parse("= nope"),
        Err(ParamsError::Syntax(_))
    ));
    for text in [
        "\"updater.promotion_ceiling_secs\" = -1\n",
        "\"updater.promotion_ceiling_secs\" = \"soon\"\n",
        "\"updater.promotion_ceiling_secs\" = 1.5\n",
    ] {
        assert!(
            matches!(Params::parse(text), Err(ParamsError::BadCeiling)),
            "{text}"
        );
    }
    for error in [
        ParamsError::Syntax("x".to_owned()),
        ParamsError::NotScalar("k".to_owned()),
        ParamsError::BadCeiling,
        ParamsError::Read(std::io::Error::other("x")),
    ] {
        assert!(!error.to_string().is_empty());
    }
}

/// Catches a file reader that does not read the file it is given.
#[test]
fn params_are_read_from_the_file_the_updater_names() {
    let scratch = support::Scratch::new("params-file");
    let file = scratch.path().join("params.toml");
    std::fs::write(&file, "cordon_lease_secs = 300\n").unwrap();
    assert_eq!(
        Params::read(&file).unwrap().get_u64("cordon_lease_secs"),
        Some(300)
    );
    assert!(matches!(
        Params::read(&scratch.path().join("absent.toml")),
        Err(ParamsError::Read(_))
    ));
}

// --- what runs next ---

fn report(class: Class, reversible: bool, retry: Option<u64>) -> Report {
    Report {
        class,
        reversible,
        retry_after_secs: retry,
        ..Report::default()
    }
}

fn failed(phase: Phase, class: Class) -> Verdict {
    Verdict::Failed { phase, class }
}

fn rollback_then(then: Verdict) -> Next {
    Next::Rollback { then }
}

fn ctx(committed: bool, retries_left: bool, deadline: bool) -> Context {
    Context {
        committed,
        retries_left,
        verify_deadline_passed: deadline,
        ..Context::default()
    }
}

const RELEASE: Class = Class::Release;
const ENV: Class = Class::Environment;

/// Pins every cell of the table of the contract, row by row: what runs after each step for
/// success, failure, deferral and halt, with the phase the updater assigns.
#[test]
fn the_prepare_row() {
    let clean = Context::default();
    let plain = Report::default();
    assert_eq!(
        next(Step::Prepare, Outcome::Success, &plain, &clean),
        Next::Run(Step::Cutover)
    );
    // failure: rollback, then Failed(before_commit)
    assert_eq!(
        next(Step::Prepare, Outcome::Failure, &plain, &clean),
        rollback_then(failed(Phase::BeforeCommit, RELEASE))
    );
    // ... or after_commit if a step reported reversible: false (this one, or an earlier one)
    assert_eq!(
        next(
            Step::Prepare,
            Outcome::Failure,
            &report(ENV, false, None),
            &clean
        ),
        rollback_then(failed(Phase::AfterCommit, ENV))
    );
    assert_eq!(
        next(
            Step::Prepare,
            Outcome::Failure,
            &plain,
            &ctx(true, false, false)
        ),
        rollback_then(failed(Phase::AfterCommit, RELEASE))
    );
    // deferred: rollback, then Deferred with the hook's backoff
    assert_eq!(
        next(
            Step::Prepare,
            Outcome::Deferred,
            &report(RELEASE, true, Some(300)),
            &clean
        ),
        rollback_then(Verdict::Deferred {
            retry_after_secs: Some(300)
        })
    );
    assert_eq!(
        next(Step::Prepare, Outcome::Deferred, &plain, &clean),
        rollback_then(Verdict::Deferred {
            retry_after_secs: None
        })
    );
    assert_eq!(
        next(Step::Prepare, Outcome::Halt, &plain, &clean),
        Next::Report(Verdict::Halt)
    );
}

#[test]
fn the_cutover_row() {
    let clean = Context::default();
    let plain = Report::default();
    assert_eq!(
        next(Step::Cutover, Outcome::Success, &plain, &clean),
        Next::Run(Step::Verify)
    );
    // a cutover that reports reversible: false and succeeds is the commit: verify still runs
    assert_eq!(
        next(
            Step::Cutover,
            Outcome::Success,
            &report(RELEASE, false, None),
            &clean
        ),
        Next::Run(Step::Verify)
    );
    assert_eq!(
        next(Step::Cutover, Outcome::Failure, &plain, &clean),
        rollback_then(failed(Phase::BeforeCommit, RELEASE))
    );
    assert_eq!(
        next(
            Step::Cutover,
            Outcome::Failure,
            &report(RELEASE, false, None),
            &clean
        ),
        rollback_then(failed(Phase::AfterCommit, RELEASE))
    );
    assert_eq!(
        next(
            Step::Cutover,
            Outcome::Deferred,
            &report(RELEASE, true, Some(60)),
            &clean
        ),
        rollback_then(Verdict::Deferred {
            retry_after_secs: Some(60)
        })
    );
    // a deferral after something irreversible is not valid: a failure after the commit
    assert_eq!(
        next(
            Step::Cutover,
            Outcome::Deferred,
            &report(RELEASE, false, Some(60)),
            &clean
        ),
        rollback_then(failed(Phase::AfterCommit, RELEASE))
    );
    assert_eq!(
        next(
            Step::Cutover,
            Outcome::Deferred,
            &plain,
            &ctx(true, false, false)
        ),
        rollback_then(failed(Phase::AfterCommit, RELEASE))
    );
    assert_eq!(
        next(Step::Cutover, Outcome::Halt, &plain, &clean),
        Next::Report(Verdict::Halt)
    );
}

#[test]
fn the_verify_row() {
    let clean = Context::default();
    let plain = Report::default();
    assert_eq!(
        next(Step::Verify, Outcome::Success, &plain, &clean),
        Next::Run(Step::Cleanup)
    );
    // failure: rollback, then Failed with the hook's class, at the gate before the commit
    assert_eq!(
        next(Step::Verify, Outcome::Failure, &plain, &clean),
        rollback_then(failed(Phase::GateRelease, RELEASE))
    );
    assert_eq!(
        next(
            Step::Verify,
            Outcome::Failure,
            &report(ENV, true, None),
            &clean
        ),
        rollback_then(failed(Phase::GateEnvironment, ENV))
    );
    // ... and after the commit whatever the class
    assert_eq!(
        next(
            Step::Verify,
            Outcome::Failure,
            &report(ENV, true, None),
            &ctx(true, false, false)
        ),
        rollback_then(failed(Phase::AfterCommit, ENV))
    );
    // deferred: verify again with the hook's backoff until the deadline
    assert_eq!(
        next(
            Step::Verify,
            Outcome::Deferred,
            &report(RELEASE, true, Some(5)),
            &clean
        ),
        Next::Again {
            step: Step::Verify,
            after_secs: Some(5)
        }
    );
    assert_eq!(
        next(Step::Verify, Outcome::Deferred, &plain, &clean),
        Next::Again {
            step: Step::Verify,
            after_secs: None
        }
    );
    // ... then rollback, as for a failure
    assert_eq!(
        next(
            Step::Verify,
            Outcome::Deferred,
            &plain,
            &ctx(false, false, true)
        ),
        rollback_then(failed(Phase::GateRelease, RELEASE))
    );
    assert_eq!(
        next(
            Step::Verify,
            Outcome::Deferred,
            &report(ENV, true, Some(5)),
            &ctx(false, false, true)
        ),
        rollback_then(failed(Phase::GateEnvironment, ENV))
    );
    assert_eq!(
        next(
            Step::Verify,
            Outcome::Deferred,
            &plain,
            &ctx(true, false, true)
        ),
        rollback_then(failed(Phase::AfterCommit, RELEASE))
    );
    assert_eq!(
        next(Step::Verify, Outcome::Halt, &plain, &clean),
        Next::Report(Verdict::Halt)
    );
}

#[test]
fn the_rollback_row() {
    let plain = Report::default();
    // success: report the original Failed or Deferred
    for then in [
        failed(Phase::BeforeCommit, RELEASE),
        failed(Phase::GateEnvironment, ENV),
        Verdict::Deferred {
            retry_after_secs: Some(9),
        },
    ] {
        let with_then = Context {
            then: Some(then.clone()),
            ..Context::default()
        };
        assert_eq!(
            next(Step::Rollback, Outcome::Success, &plain, &with_then),
            Next::Report(then)
        );
    }
    // a rollback the updater started itself, with no verdict recorded: a failure
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Success,
            &plain,
            &Context::default()
        ),
        Next::Report(failed(Phase::BeforeCommit, RELEASE))
    );
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Success,
            &plain,
            &ctx(true, false, false)
        ),
        Next::Report(failed(Phase::AfterCommit, RELEASE))
    );
    // failure before the commit: halt, a host nothing automatic can repair
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Failure,
            &plain,
            &Context::default()
        ),
        Next::Report(Verdict::Halt)
    );
    // after the commit: exit 1 with reversible: false is Failed(after_commit) ...
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Failure,
            &report(ENV, false, None),
            &ctx(true, false, false)
        ),
        Next::Report(failed(Phase::AfterCommit, ENV))
    );
    // ... but exit 1 with no line, or reversible: true, cannot be repaired: halt
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Failure,
            &plain,
            &ctx(true, false, false)
        ),
        Next::Report(Verdict::Halt)
    );
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Failure,
            &report(RELEASE, true, None),
            &ctx(true, true, false)
        ),
        Next::Report(Verdict::Halt)
    );
    // deferred: rollback again within its bound, then as for failure
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Deferred,
            &report(RELEASE, true, Some(3)),
            &ctx(false, true, false)
        ),
        Next::Again {
            step: Step::Rollback,
            after_secs: Some(3)
        }
    );
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Deferred,
            &plain,
            &ctx(false, false, false)
        ),
        Next::Report(Verdict::Halt)
    );
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Deferred,
            &report(RELEASE, false, Some(3)),
            &ctx(true, false, false)
        ),
        Next::Report(failed(Phase::AfterCommit, RELEASE))
    );
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Halt,
            &plain,
            &ctx(true, true, false)
        ),
        Next::Report(Verdict::Halt)
    );
}

#[test]
fn the_cleanup_row() {
    let plain = Report::default();
    // success: Succeeded with promotion_pending from the last verify
    for pending in [false, true] {
        let c = Context {
            promotion_pending: pending,
            ..Context::default()
        };
        assert_eq!(
            next(Step::Cleanup, Outcome::Success, &plain, &c),
            Next::Report(Verdict::Succeeded {
                promotion_pending: pending
            })
        );
    }
    // failure: cleanup again while retries last, then Failed(after_commit); never rollback
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Failure,
            &plain,
            &ctx(true, true, false)
        ),
        Next::Again {
            step: Step::Cleanup,
            after_secs: None
        }
    );
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Failure,
            &report(ENV, false, None),
            &ctx(true, false, false)
        ),
        Next::Report(failed(Phase::AfterCommit, ENV))
    );
    // deferred: later, within the same retries
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Deferred,
            &report(RELEASE, false, Some(40)),
            &ctx(true, true, false)
        ),
        Next::Again {
            step: Step::Cleanup,
            after_secs: Some(40)
        }
    );
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Deferred,
            &plain,
            &ctx(true, false, false)
        ),
        Next::Report(failed(Phase::AfterCommit, RELEASE))
    );
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Halt,
            &plain,
            &ctx(true, true, false)
        ),
        Next::Report(Verdict::Halt)
    );
}

/// Catches two rules that hold in every context: a `halt` runs nothing further, and `cleanup`
/// is never followed by `rollback` (the commit has happened).
#[test]
fn a_halt_runs_nothing_and_cleanup_is_never_rolled_back() {
    let reports = [
        Report::default(),
        report(ENV, false, Some(1)),
        report(RELEASE, true, None),
    ];
    for step in Step::ALL {
        for outcome in [
            Outcome::Success,
            Outcome::Failure,
            Outcome::Deferred,
            Outcome::Halt,
        ] {
            for report in &reports {
                for bits in 0..16u8 {
                    let c = Context {
                        committed: bits & 1 != 0,
                        promotion_pending: bits & 2 != 0,
                        verify_deadline_passed: bits & 4 != 0,
                        retries_left: bits & 8 != 0,
                        then: None,
                    };
                    let got = next(step, outcome, report, &c);
                    if outcome == Outcome::Halt {
                        assert_eq!(got, Next::Report(Verdict::Halt), "{step} {c:?}");
                    }
                    if step == Step::Cleanup {
                        assert!(
                            !matches!(got, Next::Rollback { .. }),
                            "{step} {outcome:?} {c:?}"
                        );
                    }
                }
            }
        }
    }
}

/// A report whose line stated `reversible`, as a hook that knows the field writes it.
fn stated(class: Class, reversible: bool) -> Report {
    Report {
        class,
        reversible,
        reversible_stated: true,
        ..Report::default()
    }
}

/// A `cleanup` that fails and states `reversible: true` has committed nothing: the updater runs
/// `rollback`, and the host's result is the rollback's outcome. It does not retry first, and it
/// is not Failed(after_commit).
#[test]
fn a_failed_cleanup_that_states_reversible_true_is_rolled_back() {
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Failure,
            &stated(RELEASE, true),
            &ctx(false, true, false)
        ),
        rollback_then(failed(Phase::GateRelease, RELEASE))
    );
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Failure,
            &stated(ENV, true),
            &ctx(false, false, false)
        ),
        rollback_then(failed(Phase::GateEnvironment, ENV))
    );
}

/// A `cleanup` that fails and states `reversible: false` keeps the path of the table: run it
/// again while retries last, then Failed(after_commit); never a rollback.
#[test]
fn a_failed_cleanup_that_states_reversible_false_is_retried_then_failed_after_the_commit() {
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Failure,
            &stated(RELEASE, false),
            &ctx(false, true, false)
        ),
        Next::Again {
            step: Step::Cleanup,
            after_secs: None
        }
    );
    assert_eq!(
        next(
            Step::Cleanup,
            Outcome::Failure,
            &stated(ENV, false),
            &ctx(false, false, false)
        ),
        Next::Report(failed(Phase::AfterCommit, ENV))
    );
}

/// A `cleanup` that fails with no `reversible` field is an old hook: it keeps the path of the
/// table and never triggers a rollback.
#[test]
fn a_failed_cleanup_that_leaves_reversible_out_is_never_rolled_back() {
    let absent = Report::from_stdout("{\"class\":\"release\"}").unwrap();
    assert!(absent.reversible && !absent.reversible_stated);
    for report in [Report::default(), absent] {
        assert_eq!(
            next(
                Step::Cleanup,
                Outcome::Failure,
                &report,
                &ctx(false, true, false)
            ),
            Next::Again {
                step: Step::Cleanup,
                after_secs: None
            }
        );
        assert_eq!(
            next(
                Step::Cleanup,
                Outcome::Failure,
                &report,
                &ctx(false, false, false)
            ),
            Next::Report(failed(Phase::AfterCommit, RELEASE))
        );
    }
}

/// Pins the edge of the table: a rollback that fails and says `reversible: false` is the commit
/// even when no earlier step said so, so it is Failed(after_commit), not a halt.
#[test]
fn a_rollback_that_reports_the_commit_itself_is_failed_after_the_commit() {
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Failure,
            &report(RELEASE, false, None),
            &Context::default()
        ),
        Next::Report(failed(Phase::AfterCommit, RELEASE))
    );
    assert_eq!(
        next(
            Step::Rollback,
            Outcome::Failure,
            &report(RELEASE, true, None),
            &Context::default()
        ),
        Next::Report(Verdict::Halt)
    );
}

/// Catches the updater forgetting a commit or a pending promotion between steps: `observe`
/// keeps what the table needs.
#[test]
fn the_context_remembers_the_commit_and_the_pending_promotion() {
    let mut c = Context::default();
    c.observe(Step::Prepare, &Report::default());
    assert!(!c.committed && !c.promotion_pending);
    c.observe(
        Step::Verify,
        &Report {
            promotion_pending: true,
            ..Report::default()
        },
    );
    assert!(c.promotion_pending && !c.committed);
    // the last verify decides, and a later step's own flag does not
    c.observe(
        Step::Cleanup,
        &Report {
            promotion_pending: false,
            ..Report::default()
        },
    );
    assert!(c.promotion_pending);
    c.observe(Step::Verify, &Report::default());
    assert!(!c.promotion_pending);
    c.observe(Step::Cutover, &report(RELEASE, false, None));
    assert!(c.committed);
    // a commit is not undone by a later step that says nothing
    c.observe(Step::Verify, &Report::default());
    assert!(c.committed);
}
