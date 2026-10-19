use super::*;
use crate::golden::{
    INSTALLER_TOML_FRONTEND, INSTALLER_TOML_URL, installer_toml_frontend, installer_toml_url,
};

fn problems(text: &str) -> String {
    InstallerToml::parse(text).expect_err("refused").to_string()
}

/// Catches a change to the file's keys, defaults or spelling that does not move the golden
/// files: both decode to the expected values and render back byte for byte.
#[test]
fn the_golden_files_decode_and_render_to_themselves() {
    for (text, want) in [
        (INSTALLER_TOML_URL, installer_toml_url()),
        (INSTALLER_TOML_FRONTEND, installer_toml_frontend()),
    ] {
        assert_eq!(InstallerToml::parse(text).expect("parses"), want);
        assert_eq!(want.render(), text);
    }
}

/// Catches the closed reader refusing the `database` key that `kbf-installer setup` and
/// `run --installer-config` read (so the `check` verb would call a working host's file
/// invalid), or the key lost on the way back out.
#[test]
fn the_database_key_is_accepted_and_rendered() {
    let text = "database = \"postgres:///kbf?host=/run/postgresql\"\n";
    let parsed = InstallerToml::parse(text).expect("parses");
    assert_eq!(
        parsed.database.as_deref(),
        Some("postgres:///kbf?host=/run/postgresql")
    );
    assert!(parsed.render().ends_with(text), "{}", parsed.render());
    assert_eq!(InstallerToml::default().database, None);
}

/// Catches `render` writing the database URL with Rust's Debug escaping, which is not TOML
/// (a `'` came out as `\'` and a non-ASCII character as `\u{..}`): the rendered file must
/// parse back to the same URL.
#[test]
fn a_database_url_with_quotes_and_non_ascii_renders_as_toml() {
    let url = "postgres://kbf@db/it's?application_name=caf\u{e9}\"x\\y";
    let file = InstallerToml {
        database: Some(url.to_owned()),
        ..InstallerToml::default()
    };
    let back = InstallerToml::parse(&file.render()).expect("the rendered file parses");
    assert_eq!(back.database.as_deref(), Some(url));
}

/// Catches a default that drifted from the contract (section 3): an empty file is a frontend
/// source, 900 s window, 900 s fetch, 512 MiB, 60 s between activations.
#[test]
fn an_empty_file_is_the_defaults() {
    let d = InstallerToml::parse("").expect("parses");
    assert_eq!(d, InstallerToml::default());
    assert_eq!(
        (
            d.confirm_window_s,
            d.fetch_timeout_s,
            d.max_component_bytes,
            d.min_activation_interval_s
        ),
        (900, 900, 512 << 20, 60)
    );
    assert_eq!(d.source, Source::Frontend);
}

/// Catches a key accepted that the contract does not name (a typo that silently drops a
/// floor), and a wrong type.
#[test]
fn unknown_keys_and_wrong_types_are_refused() {
    for text in [
        "min_generaton = 3",
        "[update]\nsource = \"url\"",
        "min_generation = \"3\"",
        "source_refs = \"refs/heads/main\"",
        "min_generation = -1",
    ] {
        assert!(
            matches!(InstallerToml::parse(text), Err(ConfigError::Syntax(_))),
            "{text}"
        );
    }
}

/// Catches a bound off by one in either direction, for every bounded key.
#[test]
fn bounds_are_inclusive_and_exact() {
    let ok = [
        "confirm_window_s = 120",
        "confirm_window_s = 1800",
        "fetch_timeout_s = 60",
        "fetch_timeout_s = 3600",
        "max_component_bytes = 1",
        "max_component_bytes = 1073741824",
        "min_activation_interval_s = 0",
        "min_activation_interval_s = 86400",
        "confirm_window_max_s = 120\nconfirm_window_s = 120",
    ];
    for text in ok {
        InstallerToml::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    }
    let bad = [
        ("confirm_window_s", "confirm_window_s = 119"),
        ("confirm_window_s", "confirm_window_s = 1801"),
        ("confirm_window_max_s", "confirm_window_max_s = 1801"),
        ("fetch_timeout_s", "fetch_timeout_s = 59"),
        ("fetch_timeout_s", "fetch_timeout_s = 3601"),
        ("max_component_bytes", "max_component_bytes = 0"),
        ("max_component_bytes", "max_component_bytes = 1073741825"),
        (
            "min_activation_interval_s",
            "min_activation_interval_s = 86401",
        ),
        (
            "above confirm_window_max_s",
            "confirm_window_s = 1000\nconfirm_window_max_s = 999",
        ),
    ];
    for (key, text) in bad {
        let err = problems(text);
        assert!(err.contains(key), "{text}: {err}");
    }
}

/// Catches a source that could send the installer somewhere unintended, or one that is half
/// given: http, user information, query, fragment, a path trick, a URL for the frontend source.
#[test]
fn the_source_is_https_and_consistent() {
    let bad = [
        "release_source = \"url\"",
        "release_source = \"url\"\nrelease_url = \"http://m.example\"",
        "release_source = \"url\"\nrelease_url = \"https://u:p@m.example\"",
        "release_source = \"url\"\nrelease_url = \"https://m.example?x=1\"",
        "release_source = \"url\"\nrelease_url = \"https://m.example#x\"",
        "release_source = \"url\"\nrelease_url = \"https://m.example/a/../b\"",
        "release_source = \"url\"\nrelease_url = \"https://m.example:0\"",
        "release_source = \"url\"\nrelease_url = \"https://-m.example\"",
        "release_source = \"ftp\"",
        "release_url = \"https://m.example\"",
    ];
    for text in bad {
        assert!(
            matches!(InstallerToml::parse(text), Err(ConfigError::Invalid(_))),
            "{text}"
        );
    }
    let ok = InstallerToml::parse(
        "release_source = \"url\"\nrelease_url = \"https://M.Example:8443/r/\"",
    )
    .expect("parses");
    assert_eq!(
        ok.source,
        Source::Url("https://m.example:8443/r".to_owned())
    );
}

/// Catches the node identity being half given, relative, or given to a source that has no use
/// for it.
#[test]
fn the_node_identity_is_a_pair_of_absolute_paths_for_the_frontend_source() {
    for text in [
        "node_cert = \"/etc/kbf/node.crt\"",
        "node_cert = \"node.crt\"\nnode_key = \"/k\"",
        "node_cert = \"/a/../b\"\nnode_key = \"/k\"",
        "node_cert = \"/a\\\"b\"\nnode_key = \"/k\"",
        "release_source = \"url\"\nrelease_url = \"https://m.example\"\nnode_cert = \"/c\"\nnode_key = \"/k\"",
    ] {
        assert!(
            matches!(InstallerToml::parse(text), Err(ConfigError::Invalid(_))),
            "{text}"
        );
    }
}

/// Catches a source_refs entry that is not a ref name.
#[test]
fn source_refs_are_ref_names() {
    for bad in [
        "main",
        "refs/heads/../x",
        "refs//x",
        "refs/heads/a b",
        "refs/heads/",
    ] {
        let text = format!("source_refs = [\"{bad}\"]");
        assert!(problems(&text).contains("source_refs"), "{bad}");
    }
}

/// Every problem is reported, so an operator fixes the file once.
#[test]
fn all_problems_are_reported_together() {
    let err = problems(
        "confirm_window_s = 5\nfetch_timeout_s = 5\nmax_component_bytes = 0\nrelease_source = \"url\"",
    );
    for key in [
        "confirm_window_s",
        "fetch_timeout_s",
        "max_component_bytes",
        "release_url",
    ] {
        assert!(err.contains(key), "{key}: {err}");
    }
}

#[test]
fn a_file_over_the_cap_is_refused() {
    let text = format!("# {}\n", "x".repeat(MAX_FILE_BYTES));
    assert!(matches!(
        InstallerToml::parse(&text),
        Err(ConfigError::TooLarge(_))
    ));
}

fn url_problem(url: &str) -> String {
    problems(&format!(
        "release_source = \"url\"\nrelease_url = \"{url}\""
    ))
}

/// Catches each clause of the URL check on its own: a path with a character, an empty part,
/// a `.` part; a port that is not digits, too big or zero; a host that is empty, too long,
/// has an empty or long label, a label with a leading or trailing dash or a character outside
/// `[a-z0-9-]`, or a bracketed address that is not IPv6. And the forms that must pass.
#[test]
fn every_clause_of_the_url_check_can_refuse() {
    let label64 = "a".repeat(64);
    let host254 = format!("{}.com", vec!["a".repeat(60); 5].join("."));
    for (url, want) in [
        ("https://m.example/a b", "path"),
        ("https://m.example/a%20b", "path"),
        ("https://m.example/a//b", "path"),
        ("https://m.example/a/./b", "path"),
        ("https://m.example:80a", "port"),
        ("https://m.example:70000", "port"),
        ("https://m.example:", "port"),
        ("https://:443", "host"),
        ("https://a..b", "host"),
        ("https://m-.example", "host"),
        ("https://m_x.example", "host"),
        ("https://[::g]", "host"),
        ("https://[not-an-address]", "host"),
    ] {
        assert!(url_problem(url).contains(want), "{url}");
    }
    for url in [format!("https://{label64}"), format!("https://{host254}")] {
        assert!(url_problem(&url).contains("host"), "{url}");
    }
    for (url, want) in [
        (
            "https://[2001:db8::1]:8443/r",
            "https://[2001:db8::1]:8443/r",
        ),
        ("https://[::1]", "https://[::1]"),
        ("https://192.0.2.7:8443", "https://192.0.2.7:8443"),
        (
            "https://m.example/a-b_c.d~e/f",
            "https://m.example/a-b_c.d~e/f",
        ),
    ] {
        let toml = format!("release_source = \"url\"\nrelease_url = \"{url}\"");
        assert_eq!(
            InstallerToml::parse(&toml).expect("parses").source,
            Source::Url(want.to_owned())
        );
    }
}

/// Catches each clause of the ref check: not under `refs/`, too long, a character outside
/// the set, a `..`; and the refs that must pass.
#[test]
fn every_clause_of_the_ref_check_can_refuse() {
    for bad in [
        "heads/main".to_owned(),
        format!("refs/{}", "a".repeat(200)),
        "refs/a b".to_owned(),
        "refs/a..b".to_owned(),
        "refs/a~b".to_owned(),
    ] {
        let text = format!("source_refs = [\"{bad}\"]");
        assert!(problems(&text).contains("source_refs"), "{bad}");
    }
    let ok = InstallerToml::parse("source_refs = [\"refs/heads/release-1.2_x\", \"refs/tags/v1\"]")
        .expect("parses");
    assert_eq!(ok.source_refs.len(), 2);
}

/// Catches each clause of the path check for the node identity: a space or control
/// character, a backslash, a quote, no leading slash, an empty or dot part.
#[test]
fn every_clause_of_the_path_check_can_refuse() {
    for bad in [
        "/a b", "/a\\\\b", "/a\\\"b", "a/b", "//a", "/a/./b", "/a/", "/",
    ] {
        let text = format!("node_cert = \"{bad}\"\nnode_key = \"/k\"");
        assert!(
            matches!(InstallerToml::parse(&text), Err(ConfigError::Invalid(_))),
            "{bad}"
        );
    }
}

/// A key alone is as wrong as a cert alone, and either is wrong with a URL source.
#[test]
fn the_identity_pair_and_the_source_agree() {
    assert!(problems("node_key = \"/k\"").contains("come together"));
    let url = "release_source = \"url\"\nrelease_url = \"https://m.example\"\n";
    assert!(problems(&format!("{url}node_key = \"/k\"")).contains("release_source"));
    let ok = InstallerToml::parse(
        "release_source = \"frontend\"\nnode_cert = \"/c\"\nnode_key = \"/k\"",
    );
    assert!(ok.is_ok());
}

/// Catches a control character in the database URL being written raw into the file (which
/// would break the line, or hide a key): it is escaped as `\u00XX` and reads back the same.
#[test]
fn a_control_character_in_the_database_url_is_escaped() {
    let rendered = toml_string("a\u{1}b\tc");
    assert_eq!(rendered, "\"a\\u0001b\\u0009c\"");
    let back: toml::Table = format!("k = {rendered}").parse().expect("valid TOML");
    assert_eq!(back["k"].as_str(), Some("a\u{1}b\tc"));
}

/// Catches a message that repeats text a person may have put a credential in. A syntax error
/// names its line and the parser's reason, not the line; a URL with user information, a query
/// or a fragment is refused by rule, without the URL. (`password` is what must not appear.)
///
/// Mutants to plant: `Syntax(e.to_string())`; the URL back in either refusal.
#[test]
fn a_refusal_never_repeats_the_text_that_may_hold_a_password() {
    let found = problems("confirm_window_s = 900\ndatabase = postgres://u:password@h/db\n");
    assert!(found.contains("line 2:"), "{found}");
    for text in [
        "database = postgres://u:password@h/db\n",
        "release_source = \"url\"\nrelease_url = \"https://u:password@h/x\"\n",
        "release_source = \"url\"\nrelease_url = \"https://h/x?token=password\"\n",
        "release_source = \"url\"\nrelease_url = \"https://h/x#password\"\n",
    ] {
        let found = problems(text);
        assert!(!found.contains("password"), "{text:?}: {found}");
    }
    let found = problems("release_source = \"url\"\nrelease_url = \"https://u:password@h/x\"\n");
    assert!(found.contains("must not carry user information"), "{found}");
    // The flags go through the same check.
    let found = https_base_named("--release-url", "https://u:password@h/x").expect_err("refused");
    assert!(
        !found.contains("password") && found.starts_with("--release-url"),
        "{found}"
    );
}

/// Catches a value typed into the wrong place reaching a message: the URL is planted as a
/// key, a table header, a value of the wrong type and a value of a key that checks it, and
/// neither `Display` nor `Debug` of the error may hold it. (`PW` is the password.)
///
/// Mutants to plant: `redact` returning the message as it came; a `{v:?}`, `{r:?}` or
/// `{other:?}` back in the key checks.
#[test]
fn a_value_in_the_wrong_place_is_never_repeated() {
    const URL: &str = "postgres://u:PW@h/x";
    for text in [
        format!("\"{URL}\" = 1\n"),
        format!("[\"{URL}\"]\n"),
        format!("confirm_window_s = \"{URL}\"\n"),
        format!("source_refs = [\"{URL}\"]\n"),
        format!("release_source = \"{URL}\"\n"),
        format!("node_cert = \"{URL}\"\nnode_key = \"{URL}\"\n"),
        format!("database = [\"{URL}\"]\n"),
        format!("database = {URL}\n"),
    ] {
        let err = InstallerToml::parse(&text).expect_err("refused");
        let shown = format!("{err} {err:?}");
        assert!(
            !shown.contains("PW") && !shown.contains("postgres:"),
            "{text:?}: {shown}"
        );
    }
    // The URL schemes the check meets before it knows the URL is not https.
    for url in [
        "http://u:PW@h/x",
        "HTTPS://u:PW@h/x",
        "postgres://u:PW@h/x",
        "http://h/x?token=PW",
    ] {
        let found = url_problem(url);
        assert!(!found.contains("PW"), "{url}: {found}");
        for label in ["--release-url", "--frontend-url"] {
            let found = https_base_named(label, url).expect_err("refused");
            assert!(!found.contains("PW") && found.starts_with(label), "{found}");
        }
    }
}

/// Catches the line number counted from the end of the error's span instead of its start, and
/// the arm for an error with no span (a `custom` one) going unrun.
#[test]
fn a_syntax_error_names_the_line_it_starts_on() {
    let text = "confirm_window_s = 900\nfetch_timeout_s = [\n1,\n2\n]\n";
    let found = problems(text);
    assert!(found.contains("line 2:"), "{found}");
    let custom = <toml::de::Error as serde::de::Error>::custom("a reason `with text`");
    assert_eq!(
        syntax_line("x", &custom),
        "a reason (the text is not shown)"
    );
}
