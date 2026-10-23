use super::{
    BadCommand, Channel, MAX_CHANNEL_LEN, MAX_UNIT_VERSION_LEN, MAX_VERSION_LEN, Request, Version,
    in_grammar,
};

fn parse(text: &str) -> Result<Request, BadCommand> {
    Request::parse(text)
}

fn install(text: &str) -> (String, String) {
    match parse(text) {
        Ok(Request::Install { version, channel }) => {
            (version.as_str().to_owned(), channel.as_str().to_owned())
        }
        other => panic!("{text:?}: {other:?}"),
    }
}

/// The two forms parse, and give back exactly the version and channel in them.
#[test]
fn the_two_forms_are_accepted() {
    assert_eq!(
        install("install kbf=1.2.3-1 from candidate"),
        ("1.2.3-1".into(), "candidate".into())
    );
    assert_eq!(
        install("install kbf=0.1.0~rc1+git20261001.abcdef0-1 from stable"),
        ("0.1.0~rc1+git20261001.abcdef0-1".into(), "stable".into())
    );
    assert_eq!(install("install kbf=7 from a"), ("7".into(), "a".into()));
    assert_eq!(
        install("install kbf=1 from 0-x9-"),
        ("1".into(), "0-x9-".into()),
        "digits and hyphens are channel characters"
    );
    let query = parse("query").unwrap();
    assert!(matches!(query, Request::Query));
    assert_eq!(query.verb(), "query");
    assert_eq!(parse("install kbf=1 from c").unwrap().verb(), "install");
}

/// The bounds are inclusive: 128 version bytes and 32 channel bytes are accepted.
#[test]
fn the_length_limits_are_inclusive() {
    let version = format!("1{}", "0".repeat(MAX_VERSION_LEN - 1));
    let channel = "c".repeat(MAX_CHANNEL_LEN);
    let (v, c) = install(&format!("install kbf={version} from {channel}"));
    assert_eq!((v.len(), c.len()), (MAX_VERSION_LEN, MAX_CHANNEL_LEN));
}

/// Every input below is refused. The reason each one must be refused is in the comment
/// above it.
#[test]
fn refused_inputs() {
    let long_version = format!("1{}", "0".repeat(MAX_VERSION_LEN));
    let long_channel = "c".repeat(MAX_CHANNEL_LEN + 1);
    let cases: Vec<(&str, String)> = vec![
        // Whole-string anchoring: a prefix, a suffix or a substring is not the request.
        ("query then a token", "query x".into()),
        ("query glued to text", "queryx".into()),
        ("query as a substring", "xquery".into()),
        ("query inside a sentence", "please query".into()),
        ("install as a prefix", "install kbf=1 from c; reboot".into()),
        (
            "a second install",
            "install kbf=1 from c install kbf=2 from c".into(),
        ),
        ("an install as a substring", "x install kbf=1 from c".into()),
        // An extra token after the channel, or a second argument.
        ("extra token", "install kbf=1 from c d".into()),
        ("extra token, query", "query now".into()),
        ("missing channel", "install kbf=1 from".into()),
        ("missing from", "install kbf=1 c".into()),
        ("empty channel", "install kbf=1 from ".into()),
        ("empty request", "".into()),
        ("only install", "install".into()),
        ("install without version", "install kbf from c".into()),
        // Spaces: exactly one between tokens, none at the ends.
        ("trailing space", "install kbf=1 from c ".into()),
        ("trailing space, query", "query ".into()),
        ("leading space", " install kbf=1 from c".into()),
        ("leading space, query", " query".into()),
        ("double space", "install  kbf=1 from c".into()),
        ("double space before from", "install kbf=1  from c".into()),
        ("double space after from", "install kbf=1 from  c".into()),
        ("tab for space", "install\tkbf=1 from c".into()),
        ("tab before from", "install kbf=1\tfrom c".into()),
        // Shell metacharacters in the version.
        ("semicolon in version", "install kbf=1;id from c".into()),
        ("pipe in version", "install kbf=1|id from c".into()),
        ("ampersand in version", "install kbf=1&id from c".into()),
        ("dollar in version", "install kbf=1$HOME from c".into()),
        ("backtick in version", "install kbf=1`id` from c".into()),
        ("greater-than in version", "install kbf=1>x from c".into()),
        ("less-than in version", "install kbf=1<x from c".into()),
        ("open paren in version", "install kbf=1(x from c".into()),
        ("close paren in version", "install kbf=1)x from c".into()),
        ("single quote in version", "install kbf=1'x from c".into()),
        ("double quote in version", "install kbf=1\"x from c".into()),
        ("backslash in version", "install kbf=1\\x from c".into()),
        ("equals in version", "install kbf=1=2 from c".into()),
        ("slash in version", "install kbf=1/2 from c".into()),
        ("non-ASCII in version", "install kbf=1\u{e9} from c".into()),
        // Shell metacharacters in the channel.
        ("semicolon in channel", "install kbf=1 from c;id".into()),
        ("pipe in channel", "install kbf=1 from c|id".into()),
        ("ampersand in channel", "install kbf=1 from c&id".into()),
        ("dollar in channel", "install kbf=1 from c$HOME".into()),
        ("backtick in channel", "install kbf=1 from c`id`".into()),
        ("greater-than in channel", "install kbf=1 from c>x".into()),
        ("less-than in channel", "install kbf=1 from c<x".into()),
        ("open paren in channel", "install kbf=1 from c(x".into()),
        ("close paren in channel", "install kbf=1 from c)x".into()),
        ("single quote in channel", "install kbf=1 from c'x".into()),
        ("double quote in channel", "install kbf=1 from c\"x".into()),
        ("backslash in channel", "install kbf=1 from c\\x".into()),
        // A channel is a name, not a path.
        ("dot-dot channel", "install kbf=1 from ../x".into()),
        ("dot channel", "install kbf=1 from .".into()),
        ("slash channel", "install kbf=1 from a/b".into()),
        ("absolute channel", "install kbf=1 from /etc/passwd".into()),
        ("dotted channel", "install kbf=1 from a.b".into()),
        ("upper-case channel", "install kbf=1 from Candidate".into()),
        ("underscore channel", "install kbf=1 from a_b".into()),
        ("leading hyphen channel", "install kbf=1 from -x".into()),
        (
            "over-long channel",
            format!("install kbf=1 from {long_channel}"),
        ),
        // Line ends and NUL.
        ("trailing newline", "install kbf=1 from c\n".into()),
        ("trailing newline, query", "query\n".into()),
        ("trailing CR", "install kbf=1 from c\r".into()),
        ("trailing CR LF", "query\r\n".into()),
        ("trailing NUL", "install kbf=1 from c\0".into()),
        ("trailing NUL, query", "query\0".into()),
        ("newline in version", "install kbf=1\n2 from c".into()),
        ("newline between tokens", "install kbf=1\nfrom c".into()),
        ("second line", "query\nquery".into()),
        ("CR in channel", "install kbf=1 from c\rd".into()),
        ("NUL in version", "install kbf=1\0 from c".into()),
        ("NUL in channel", "install kbf=1 from c\0d".into()),
        // A leading hyphen would be an option to apt-get.
        ("leading hyphen version", "install kbf=-1 from c".into()),
        ("option as version", "install kbf=--help from c".into()),
        ("leading hyphen verb", "-install kbf=1 from c".into()),
        // An epoch.
        ("epoch", "install kbf=1:2.0 from c".into()),
        ("epoch of zero", "install kbf=0:2.0 from c".into()),
        ("trailing colon", "install kbf=2.0: from c".into()),
        // The version itself.
        ("empty version", "install kbf= from c".into()),
        (
            "over-long version",
            format!("install kbf={long_version} from c"),
        ),
        ("letter first", "install kbf=a1 from c".into()),
        ("tilde first", "install kbf=~1 from c".into()),
        ("plus first", "install kbf=+1 from c".into()),
        ("dot first", "install kbf=.1 from c".into()),
        ("empty revision", "install kbf=1.0- from c".into()),
        ("only a hyphen", "install kbf=- from c".into()),
        // The package is fixed.
        ("wrong package", "install foo=1 from candidate".into()),
        ("package prefix", "install kbf-extra=1 from c".into()),
        ("package suffix", "install kbf2=1 from c".into()),
        ("no package", "install =1 from c".into()),
        ("package with release", "install kbf/stable=1 from c".into()),
        ("two packages", "install kbf=1 foo=2 from c".into()),
        // Case.
        ("capital Install", "Install kbf=1 from c".into()),
        ("upper-case INSTALL", "INSTALL kbf=1 from c".into()),
        ("capital Query", "Query".into()),
        ("upper-case QUERY", "QUERY".into()),
        ("capital From", "install kbf=1 From c".into()),
        ("capital KBF", "install KBF=1 from c".into()),
        // Other verbs.
        ("remove", "remove kbf".into()),
        ("a program name", "systemctl".into()),
        ("sftp", "internal-sftp".into()),
    ];
    for (what, text) in &cases {
        assert_eq!(parse(text).map(|_| ()), Err(BadCommand), "{what}: {text:?}");
    }
}

/// The version and the channel are checked on their own too, not only through `parse`.
#[test]
fn version_and_channel_alone() {
    assert!(Version::new("1.0").is_ok());
    assert!(Version::new("1.0").unwrap().parsed() < Version::new("1.1").unwrap().parsed());
    assert!(Version::new("").is_err());
    assert!(Version::new("1 0").is_err());
    assert!(Channel::new("candidate").is_ok());
    assert!(Channel::new("").is_err());
    assert!(Channel::new("a b").is_err());
}

/// A version whose unit name would pass systemd's 255-byte limit is refused here, as a
/// bad command, rather than failing later: `+` and `~` take two bytes in the name, so the
/// limit is on the expanded length.
#[test]
fn a_version_too_long_for_a_unit_name_is_refused() {
    let tildes = (MAX_UNIT_VERSION_LEN - 2) / 2;
    let at_limit = format!("11{}", "~".repeat(tildes));
    assert!(parse(&format!("install kbf={at_limit} from c")).is_ok());
    for ch in ["~", "+"] {
        let over = format!("11{}", ch.repeat(tildes + 1));
        assert!(over.len() <= MAX_VERSION_LEN, "within the byte limit");
        assert!(
            parse(&format!("install kbf={over} from c")).is_err(),
            "{ch}"
        );
    }
    // Plain characters are one byte each: the full 128 is fine.
    let plain = format!("1{}", "9".repeat(MAX_VERSION_LEN - 1));
    assert!(parse(&format!("install kbf={plain} from c")).is_ok());
}

/// The version grammar alone, without dpkg's parse behind it: `Version::new` also asks
/// `DebVersion::parse`, which refuses most of these too, so a defect in the grammar's own
/// first-character rule (the one that keeps `-` from becoming an option to `apt-get`) is
/// seen only here. The first group is outside the grammar, the second inside it.
#[test]
fn the_grammar_alone_refuses_an_option_and_a_non_digit() {
    for text in [
        "-1.0",
        "--help",
        "-",
        "-1",
        "--version=1",
        "-o",
        "~1",
        "+1",
        ".1",
        "a1",
        "v1.0",
        "",
        "1:2",
        "1 0",
        "1_0",
        "1/0",
        "1\u{e9}",
    ] {
        assert!(!in_grammar(text), "{text:?}");
    }
    for text in ["0", "1", "1.0", "1.0-1", "0.1.0~rc1+git-1", "9a"] {
        assert!(in_grammar(text), "{text:?}");
    }
    // A first character that is a digit but not an ASCII one is not a digit.
    assert!(!in_grammar("\u{661}.0"));
}

/// The limits of the grammar alone: the byte limit and the unit-name limit, each inclusive.
#[test]
fn the_grammar_alone_has_inclusive_limits() {
    assert!(in_grammar(&format!("1{}", "0".repeat(MAX_VERSION_LEN - 1))));
    assert!(!in_grammar(&format!("1{}", "0".repeat(MAX_VERSION_LEN))));
    let tildes = (MAX_UNIT_VERSION_LEN - 2) / 2;
    assert!(in_grammar(&format!("11{}", "~".repeat(tildes))));
    assert!(!in_grammar(&format!("11{}", "~".repeat(tildes + 1))));
}

/// The request parser alone: a version with a leading hyphen is not a request, whichever
/// layer refuses it. (`parse` goes through the grammar first; this pins the result.)
#[test]
fn a_leading_hyphen_is_no_request() {
    for version in ["-1.0", "--help", "-", "-1"] {
        assert!(
            Request::parse(&format!("install kbf={version} from candidate")).is_err(),
            "{version}"
        );
    }
}
