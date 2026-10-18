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
