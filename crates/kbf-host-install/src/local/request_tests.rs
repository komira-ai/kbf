//! The grammar of the local socket's six requests.

use super::request::{LocalRequest, MAX_LINE_LEN};

const VERSION: &str = "0.1.7+gbbbbbbbbbbbb-3";

/// Each of the five forms parses to its own request, with the version and channel it names,
/// and says its verb for the audit line.
#[test]
fn the_five_requests_parse_and_name_their_verbs() {
    let install = LocalRequest::parse(&format!("install kbf={VERSION} from stable")).unwrap();
    assert!(matches!(
        &install,
        LocalRequest::Install { version, channel }
            if version.as_str() == VERSION && channel.as_str() == "stable"
    ));
    let switch = LocalRequest::parse(&format!("switch kbf={VERSION}")).unwrap();
    assert!(matches!(&switch, LocalRequest::Switch { version } if version.as_str() == VERSION));
    let confirm = LocalRequest::parse(&format!("confirm kbf={VERSION}")).unwrap();
    assert!(
        matches!(&confirm, LocalRequest::Confirm { version, last_good: None } if version.as_str() == VERSION)
    );
    assert!(matches!(
        LocalRequest::parse("revert").unwrap(),
        LocalRequest::Revert
    ));
    assert!(matches!(
        LocalRequest::parse("query").unwrap(),
        LocalRequest::Query
    ));
    let verbs: Vec<_> = [
        &LocalRequest::Query,
        &install,
        &switch,
        &confirm,
        &LocalRequest::Revert,
    ]
    .iter()
    .map(|r| r.verb())
    .collect();
    assert_eq!(verbs, ["query", "stage", "switch", "confirm", "revert"]);
}

/// Whatever is not exactly one of the forms is refused: another verb, the ssh path's
/// `restart`, case, spaces and control bytes around or inside, a missing or extra part, a
/// version that is an option or has an epoch, a channel that is a path. The request never
/// carries a path or a URL.
#[test]
fn anything_else_is_refused() {
    let bad = [
        "",
        "Query",
        "query ",
        " query",
        "query\n",
        "revert now",
        "restart frontend=-",
        "install",
        "install kbf=",
        &format!("install kbf={VERSION}"),
        &format!("install kbf={VERSION} from"),
        &format!("install kbf={VERSION} from stable now"),
        &format!("install kbf={VERSION}  from stable"),
        &format!("install kbf={VERSION} from ../stable"),
        &format!("install kbf={VERSION} from Stable"),
        &format!("install kbf={VERSION} from https://example.invalid/kbf.tar.gz"),
        "install kbf=-1 from stable",
        "install kbf=1:2.0 from stable",
        &format!("switch kbf={VERSION} from stable"),
        &format!("switch  kbf={VERSION}"),
        "switch kbf=",
        "switch kbf=/tmp/kbf",
        &format!("confirm kbf={VERSION}\t"),
        &format!("confirm {VERSION}"),
        "confirm",
        "switch",
        &format!("install kbf={VERSION} from stable\0"),
    ];
    for text in bad {
        assert!(LocalRequest::parse(text).is_err(), "{text:?}");
    }
}

/// The line limit leaves room for the longest request: a version of 128 bytes and a channel
/// of 32.
#[test]
fn the_line_limit_fits_the_longest_request() {
    let version = format!("1.{}", "0".repeat(126));
    let channel = "c".repeat(32);
    let longest = format!("install kbf={version} from {channel}");
    assert!(longest.len() <= MAX_LINE_LEN, "{}", longest.len());
    assert!(LocalRequest::parse(&longest).is_ok());
}

/// `activate kbf=` takes a commit (12 to 40 lower-case hex digits) or a version, says its verb,
/// and carries the text as given; anything else after it is not a request.
///
/// Catches: a commit shorter than 12 digits taken as a prefix; an upper-case or 41-digit commit
/// taken; a pin or a lock flag accepted on the socket.
#[test]
fn activate_takes_a_commit_or_a_version() {
    use super::request::Target;
    let by_commit = LocalRequest::parse("activate kbf=bbbbbbbbbbbb").unwrap();
    assert_eq!(by_commit.verb(), "activate");
    assert!(matches!(
        &by_commit,
        LocalRequest::Activate { target: Target::Commit(c) } if c == "bbbbbbbbbbbb"
    ));
    let by_version = LocalRequest::parse(&format!("activate kbf={VERSION}")).unwrap();
    assert!(matches!(
        &by_version,
        LocalRequest::Activate { target: Target::Version(v) } if v.as_str() == VERSION
    ));
    let full = "b".repeat(40);
    assert_eq!(Target::parse(&full).unwrap().as_str(), full);
    // Eleven digits are a version (a digit first), never a commit.
    assert!(matches!(
        Target::parse("12345678901").unwrap(),
        Target::Version(_)
    ));
    for bad in [
        "activate kbf=bbbbbbbbbbb".to_owned(),
        "activate kbf=BBBBBBBBBBBB".to_owned(),
        format!("activate kbf={}", "b".repeat(41)),
        format!("activate kbf={VERSION} --pin"),
        "activate kbf=".to_owned(),
        "activate".to_owned(),
    ] {
        assert!(LocalRequest::parse(&bad).is_err(), "{bad}");
    }
}

/// A target names a release by the start of its commit, by the version text the host recorded
/// for it, or by the commit a version carries; nothing else.
///
/// Catches: a version matched only by its text, so a release whose text the host lost cannot be
/// named; a prefix matched anywhere but at the start.
#[test]
fn a_target_names_a_release_by_commit_or_by_version() {
    use super::request::Target;
    let b = "b".repeat(40);
    let named = |text: &str, commit: &str, version: Option<&str>| {
        Target::parse(text).unwrap().names(commit, version)
    };
    assert!(named("bbbbbbbbbbbb", &b, None));
    assert!(!named("abbbbbbbbbbb", &b, None));
    assert!(named(VERSION, &b, None), "the commit the version carries");
    assert!(
        named(VERSION, &"c".repeat(40), Some(VERSION)),
        "the recorded text"
    );
    assert!(!named(VERSION, &"c".repeat(40), Some("0.1.7-3")));
    assert!(named("0.1.7-3", &"c".repeat(40), Some("0.1.7-3")));
    assert!(
        !named("0.1.7-3", &"c".repeat(40), None),
        "no commit, no text"
    );
}
