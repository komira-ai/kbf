use super::*;
use crate::golden::{
    INSTALLER_TOML_BUNDLED, INSTALLER_TOML_FRONTEND, INSTALLER_TOML_FRONTEND_PULL,
    INSTALLER_TOML_URL, INSTALLER_TOML_URL_CA, installer_toml_bundled, installer_toml_frontend,
    installer_toml_frontend_pull, installer_toml_url, installer_toml_url_ca,
};

fn problems(text: &str) -> String {
    InstallerToml::parse(text).expect_err("refused").to_string()
}

/// Catches a change to the file's keys, defaults or spelling that does not move the golden
/// files: both decode to the expected values and render back byte for byte. The first two
/// are files written before `bundled`, the snapshot and `release_ca` (they decode as they did);
/// the other three carry the new keys, in the order `render` writes them.
///
/// Mutants to plant: `render` writing the snapshot before `node_cert`, or `release_ca` after
/// the limits; a key dropped from `render`; `bundled` spelt otherwise.
#[test]
fn the_golden_files_decode_and_render_to_themselves() {
    for (text, want) in [
        (INSTALLER_TOML_URL, installer_toml_url()),
        (INSTALLER_TOML_FRONTEND, installer_toml_frontend()),
        (INSTALLER_TOML_BUNDLED, installer_toml_bundled()),
        (INSTALLER_TOML_FRONTEND_PULL, installer_toml_frontend_pull()),
        (INSTALLER_TOML_URL_CA, installer_toml_url_ca()),
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

mod read {
    use kbf_host::exec::Entry;
    use kbf_host::path::HostPath;
    use kbf_host::step::{Owner, Perms};
    use kbf_host::testkit::FakeExec;

    use super::*;

    const FILE: &str = "/etc/kbf/installer.toml";
    const ROOT: Owner = Owner { uid: 0, gid: 0 };

    fn dir(mode: u32, owner: Owner) -> Entry {
        Entry::Dir {
            perms: Perms::new(mode),
            owner,
        }
    }

    fn file(bytes: &[u8], mode: u32, owner: Owner) -> Entry {
        Entry::File {
            perms: Perms::new(mode),
            owner,
            bytes: bytes.to_vec(),
        }
    }

    /// A host as `setup` leaves it: root's 0644 file in root's 0755 `/etc/kbf` in `/etc`.
    fn host(text: &str) -> FakeExec {
        let mut exec = FakeExec::new(0);
        for at in ["/etc", "/etc/kbf"] {
            exec.entries
                .insert(HostPath::new(at).expect("path"), dir(0o755, ROOT));
        }
        exec.entries.insert(
            HostPath::new(FILE).expect("path"),
            file(text.as_bytes(), 0o644, ROOT),
        );
        exec
    }

    fn read(exec: &FakeExec) -> Result<InstallerToml, ReadError> {
        InstallerToml::read(exec, &HostPath::new(FILE).expect("path"))
    }

    /// The word of a refusal: the code `Refusal` gives it, or `invalid` for a file that is not
    /// an `installer.toml`.
    fn why(result: Result<InstallerToml, ReadError>) -> String {
        match result.expect_err("refused") {
            ReadError::Untrusted(refusal) => refusal.code().to_owned(),
            ReadError::Invalid(e) => format!("invalid: {e}"),
        }
    }

    /// A root-owned file nobody else can write is read and parsed as `parse` would.
    ///
    /// Catches a reader that returns the defaults for a file it did read, and one that parses
    /// something other than the file's bytes.
    #[test]
    fn a_trusted_file_is_read_and_parsed() {
        let text = "release_source = \"url\"\nrelease_url = \"https://mirror.example\"\nmin_protocol = 3\n";
        let got = read(&host(text)).expect("trusted");
        assert_eq!(got, InstallerToml::parse(text).expect("parses"));
        assert_eq!(got.min_protocol, 3);
    }

    /// The file decides where releases come from and which are refused, so whoever can write
    /// it decides: a file owned by anyone but root, or writable by group or others, or in a
    /// directory that is, is refused with the trust rule's code, and nothing of its content is
    /// used (it holds a valid file that a hostile owner would write).
    ///
    /// Catches the check removed (the farm's `/etc/kbf` is root's, but a mis-set one would
    /// pass), the owner check on the file alone (not its directory), and a mode mask that
    /// lets group or others write.
    ///
    /// Mutants planted: `trusted_chain` call removed; the chain asked about the file only.
    #[test]
    fn a_file_root_does_not_alone_control_is_refused() {
        let text = "release_source = \"url\"\nrelease_url = \"https://mirror.example\"\n";
        let user = Owner {
            uid: 1000,
            gid: 1000,
        };
        let mut owned = host(text);
        owned.entries.insert(
            HostPath::new(FILE).expect("path"),
            file(text.as_bytes(), 0o644, user),
        );
        assert_eq!(why(read(&owned)), "untrusted-owner");

        for mode in [0o664, 0o646, 0o666] {
            let mut open = host(text);
            open.entries.insert(
                HostPath::new(FILE).expect("path"),
                file(text.as_bytes(), mode, ROOT),
            );
            assert_eq!(why(read(&open)), "untrusted-writable", "{mode:o}");
        }

        let mut above = host(text);
        above
            .entries
            .insert(HostPath::new("/etc/kbf").expect("path"), dir(0o775, ROOT));
        assert_eq!(why(read(&above)), "untrusted-writable", "the directory");
        above
            .entries
            .insert(HostPath::new("/etc/kbf").expect("path"), dir(0o755, user));
        assert_eq!(
            why(read(&above)),
            "untrusted-owner",
            "the directory's owner"
        );
    }

    /// A file that is not there, one that cannot be read, and a directory or a link where it
    /// should be are each refused as untrusted (never read as the defaults: a host whose file
    /// is unreadable must not fetch from the default source).
    ///
    /// Catches a missing file read as the defaults and an I/O error swallowed.
    #[test]
    fn a_missing_unreadable_or_misplaced_file_is_refused() {
        let mut exec = host("");
        exec.entries.remove(&HostPath::new(FILE).expect("path"));
        assert_eq!(why(read(&exec)), "untrusted-missing");
        let err = InstallerToml::read(&exec, &HostPath::new(FILE).expect("path"))
            .expect_err("missing")
            .to_string();
        assert_eq!(err, format!("{FILE} is not trusted: it is missing"));

        let unreadable = HostPath::new("/unreadable").expect("path");
        let got = InstallerToml::read(&host(""), &unreadable);
        assert_eq!(why(got), "untrusted-unreadable");

        exec.entries
            .insert(HostPath::new(FILE).expect("path"), dir(0o755, ROOT));
        assert_eq!(why(read(&exec)), "untrusted-link");
    }

    /// A trusted file that is not an `installer.toml` is refused with the parser's own reason
    /// (every problem it lists), and one that is not UTF-8 is refused rather than read lossily.
    ///
    /// Catches parse errors lost on the way, and bytes turned into replacement characters.
    #[test]
    fn a_trusted_file_with_bad_content_is_refused_with_the_parsers_reason() {
        let got = why(read(&host("release_source = \"carrier-pigeon\"\n")));
        assert!(
            got.starts_with("invalid: installer.toml:") && got.contains("release_source"),
            "{got}"
        );
        let mut exec = host("");
        exec.entries.insert(
            HostPath::new(FILE).expect("path"),
            file(b"database = \"\xff\"\n", 0o644, ROOT),
        );
        assert_eq!(why(read(&exec)), "invalid: installer.toml: it is not UTF-8");
    }
}

/// The `installer.toml` of a proxy host, as `setup --proxy` writes it with every flag at its
/// default.
const PROXY_HOST: &str = include_str!("../tests/golden/proxy_host/installer.toml");

/// Catches a `[proxy]` table that `render` drops or spells differently from the proxy's own
/// parser (the bug that made `setup` delete a hand-written table): the golden decodes to a
/// table, renders back byte for byte, and a table with every key off its default (bearer
/// auth with its key file, other paths, limits) survives a render and a parse unchanged.
#[test]
fn the_proxy_table_is_rendered_and_read_back_with_every_key() {
    let parsed = InstallerToml::parse(PROXY_HOST).expect("parses");
    assert!(parsed.proxy.is_some());
    assert_eq!(parsed.render(), PROXY_HOST);

    let odd = PROXY_HOST
        .replace(
            "buck2_auth = \"none\"",
            "buck2_auth = \"bearer\"\nbuck2_token_file = \"/etc/kbf/bearer\"",
        )
        .replace("/run/kbf-proxy/proxy-auth", "/run/other/auth")
        .replace("/var/lib/kbf-proxy", "/var/lib/other")
        .replace("hold_max_s = 20", "hold_max_s = 7")
        .replace("ui = true", "ui = false")
        .replace("base_port = 18000", "base_port = 19010")
        .replace("max_streams = 8192", "max_streams = 100")
        .replace("max_connections = 50000", "max_connections = 5000")
        .replace(
            "max_connections_per_ip = 2000",
            "max_connections_per_ip = 20",
        )
        .replace("pre_stream_timeout_s = 10", "pre_stream_timeout_s = 11")
        .replace("memory_budget_mib = 8192", "memory_budget_mib = 4096")
        .replace(
            "max_header_list_bytes = 16384",
            "max_header_list_bytes = 8192",
        )
        .replace("keepalive_interval_s = 30", "keepalive_interval_s = 31")
        .replace("keepalive_ack_s = 20", "keepalive_ack_s = 21");
    let back = InstallerToml::parse(&odd).expect("parses");
    assert_eq!(back.proxy.as_ref().map(|p| p.ui), Some(false));
    assert_eq!(back.render(), odd);
    assert_eq!(InstallerToml::parse(&back.render()).expect("again"), back);
}

/// Catches `remote_operator_file` lost on a rewrite: `setup` parses `installer.toml` and renders
/// it again, so a key that is read and not rendered switches the CI credential off silently on
/// the next run. The key parses, survives `render`, and a table without it renders without it.
#[test]
fn the_remote_operator_file_survives_a_rewrite() {
    let with = PROXY_HOST.replace(
        "proxy_auth_file =",
        "remote_operator_file = \"/etc/kbf/remote-ci.toml\"\nproxy_auth_file =",
    );
    assert_ne!(with, PROXY_HOST);
    let parsed = InstallerToml::parse(&with).expect("parses");
    assert_eq!(
        parsed
            .proxy
            .as_ref()
            .and_then(|p| p.remote_operator_file.as_deref()),
        Some(std::path::Path::new("/etc/kbf/remote-ci.toml"))
    );
    assert_eq!(parsed.render(), with);
    assert_eq!(
        InstallerToml::parse(&parsed.render()).expect("again"),
        parsed
    );
    assert!(
        !InstallerToml::parse(PROXY_HOST)
            .expect("parses")
            .render()
            .contains("remote_operator_file")
    );
}

/// Catches a UI that is off unless asked for: the golden says `ui = true` and parses to it, and
/// a table written before the key existed (no `ui`) reads as on and renders `ui = true`, so the
/// browser UI is on for a host that never named it.
#[test]
fn a_proxy_table_has_the_ui_on_unless_it_says_off() {
    assert!(PROXY_HOST.contains("\nui = true\n"));
    let parsed = InstallerToml::parse(PROXY_HOST).expect("parses");
    assert_eq!(parsed.proxy.as_ref().map(|p| p.ui), Some(true));

    let old = PROXY_HOST.replace("ui = true\n", "");
    assert!(!old.contains("ui ="));
    let read = InstallerToml::parse(&old).expect("parses");
    assert_eq!(read.proxy.as_ref().map(|p| p.ui), Some(true));
    assert_eq!(read.render(), PROXY_HOST);
}

/// Catches a file with no `[proxy]` table growing one: a host that has none renders none.
#[test]
fn no_proxy_table_renders_no_proxy_keys() {
    assert!(!InstallerToml::default().render().contains("proxy"));
}

const PROXY: &str = r#"
[proxy]
listen = "192.0.2.10:8443"
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"

[proxy.upstream]
base_port = 18000
"#;

/// Catches `render` dropping the operator's `[proxy]` table (a `setup` run that rewrites the
/// file would erase it, and with it `listen`): the rendered file parses back to the same
/// table, and a second render is byte for byte the first.
#[test]
fn the_proxy_table_survives_a_render() {
    let parsed = InstallerToml::parse(PROXY).expect("parses");
    let listen = parsed.proxy.as_ref().and_then(|p| p.listen);
    assert_eq!(listen, Some("192.0.2.10:8443".parse().unwrap()));
    let text = parsed.render();
    assert_eq!(InstallerToml::parse(&text).expect("parses back"), parsed);
    assert_eq!(InstallerToml::parse(&text).unwrap().render(), text);
    assert!(
        text.contains("\n[proxy]\n") && text.contains("[proxy.upstream]"),
        "{text}"
    );
}

/// Catches `listen` reaching the file unchecked, and the message echoing the value: the
/// refusal names the key, as the file's others do.
#[test]
fn a_bad_proxy_listen_names_the_key_not_the_value() {
    let why = problems(&PROXY.replace("192.0.2.10:8443", "farm.example:8443"));
    assert!(why.contains("listen"), "{why}");
    assert!(!why.contains("farm.example"), "{why}");
}
