//! The grammar of the local socket's five requests.

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
    assert!(matches!(&confirm, LocalRequest::Confirm { version } if version.as_str() == VERSION));
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

/// Whatever is not exactly one of the five forms is refused: another verb, the ssh path's
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

use super::request::Addressed;

fn service_of(text: &str) -> (String, LocalRequest) {
    let addressed = Addressed::parse(text).unwrap();
    (addressed.service.as_str().to_owned(), addressed.request)
}

/// Today's five lines mean kbf, and mean what they meant: the same request `LocalRequest`
/// gives.
#[test]
fn todays_lines_are_kbfs() {
    for text in [
        "query".to_owned(),
        "revert".to_owned(),
        format!("install kbf={VERSION} from stable"),
        format!("switch kbf={VERSION}"),
        format!("confirm kbf={VERSION}"),
    ] {
        let (service, request) = service_of(&text);
        assert_eq!(service, "kbf", "{text}");
        assert_eq!(
            format!("{request:?}"),
            format!("{:?}", LocalRequest::parse(&text).unwrap()),
            "{text}"
        );
    }
}

/// The form of another service puts its name where `kbf` is, or after `query` and `revert`;
/// the rest of the line is the grammar of kbf's.
#[test]
fn a_service_is_named_where_kbf_is() {
    let (service, request) = service_of(&format!("install js-agent={VERSION} from js-stable"));
    assert_eq!(service, "js-agent");
    assert!(matches!(
        &request,
        LocalRequest::Install { version, channel }
            if version.as_str() == VERSION && channel.as_str() == "js-stable"
    ));
    let (service, request) = service_of(&format!("switch js-agent={VERSION}"));
    assert_eq!(service, "js-agent");
    assert!(matches!(&request, LocalRequest::Switch { version } if version.as_str() == VERSION));
    let (service, request) = service_of(&format!("confirm js-agent={VERSION}"));
    assert_eq!(service, "js-agent");
    assert!(matches!(&request, LocalRequest::Confirm { version } if version.as_str() == VERSION));
    let (service, request) = service_of("query js-agent");
    assert_eq!(service, "js-agent");
    assert!(matches!(request, LocalRequest::Query));
    let (service, request) = service_of("revert js-agent");
    assert_eq!(service, "js-agent");
    assert!(matches!(request, LocalRequest::Revert));
    // Naming kbf is naming kbf.
    assert_eq!(service_of("query kbf").0, "kbf");
    assert_eq!(service_of("revert kbf").0, "kbf");
}

/// A service form with a bad name, a bad rest, a verb that has none, or an extra part is
/// refused, as are the ways to smuggle a second request or a path in.
#[test]
fn a_service_form_outside_the_grammar_is_refused() {
    let bad = [
        "query ".to_owned(),
        "query Js".to_owned(),
        "query js agent".to_owned(),
        "query js-agent ".to_owned(),
        "query js-agent\n".to_owned(),
        "query js-".to_owned(),
        "query -js".to_owned(),
        "query ../js".to_owned(),
        "revert js-agent now".to_owned(),
        "revert ".to_owned(),
        "restart js-agent=-".to_owned(),
        "stage js-agent=1".to_owned(),
        "install =".to_owned(),
        "install js-agent".to_owned(),
        format!("install JS={VERSION} from stable"),
        format!("install js/agent={VERSION} from stable"),
        format!("install js-agent={VERSION}"),
        format!("install js-agent={VERSION} from ../stable"),
        "install js-agent=-1 from stable".to_owned(),
        format!("switch js-agent={VERSION} from stable"),
        format!("switch js-agent={VERSION}\0"),
        format!("confirm js-agent {VERSION}"),
        format!("confirm  js-agent={VERSION}"),
        format!("install kbf js-agent={VERSION} from stable"),
        "install kbf=".to_owned(),
    ];
    for text in bad {
        assert!(Addressed::parse(&text).is_err(), "{text:?}");
    }
}
