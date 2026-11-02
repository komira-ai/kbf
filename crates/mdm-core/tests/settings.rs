//! The settings file format: a closed `KEY=VALUE` set.

use mdm_core::settings::{Settings, SettingsError};

const KEYS: &[&str] = &["MDM_A", "MDM_B_2", "MDM_C"];

fn refusal(text: &str) -> SettingsError {
    Settings::parse(text, KEYS).expect_err("refused")
}

/// Catches a parser that takes a typo'd key (the setting would be silently ignored),
/// lets a second line override the first, or reads a line that is not `KEY=VALUE`
/// (including a key that is lower-case, starts with a digit, holds `-` or a space, or
/// is empty).
#[test]
fn unknown_repeated_and_malformed_lines_are_refused() {
    assert_eq!(
        refusal("MDM_AA=a\n"),
        SettingsError::Unknown {
            line: 1,
            key: "MDM_AA".to_owned()
        }
    );
    assert_eq!(
        refusal("MDM_A=1\n# c\nMDM_A=2\n"),
        SettingsError::Repeated {
            line: 3,
            key: "MDM_A".to_owned(),
            first: 1
        }
    );
    for text in [
        "MDM_A 64\n",
        "\nmdm_a=64\n",
        "1MDM=1\n",
        "MDM-A=1\n",
        "=1\n",
        " MDM_A=1\n",
    ] {
        let line = text.lines().position(|l| !l.is_empty()).expect("a line") + 1;
        assert_eq!(refusal(text), SettingsError::Malformed { line }, "{text:?}");
    }
    let said = |text: &str| refusal(text).to_string();
    assert_eq!(said("MDM_A 64\n"), "line 1: not KEY=VALUE");
    assert_eq!(said("MDM_AA=a\n"), "line 1: unknown key MDM_AA");
    assert_eq!(
        said("MDM_A=1\n# c\nMDM_A=2\n"),
        "line 3: MDM_A is already set on line 1"
    );
}

/// Catches blank lines or comments being read as settings, quotes kept in a value, a
/// lone quote stripped, and a value cut at a second `=`.
#[test]
fn comments_blank_lines_and_one_pair_of_quotes_are_handled() {
    let s = Settings::parse(
        "# comment\n\nMDM_A=plain\nMDM_B_2=\"two words\"\nMDM_C=\"open\n",
        KEYS,
    )
    .expect("parses");
    assert_eq!(s.get("MDM_A"), Some("plain"));
    assert_eq!(s.get("MDM_B_2"), Some("two words"));
    assert_eq!(s.get("MDM_C"), Some("\"open"));
    let s = Settings::parse("MDM_A=a=b\nMDM_C=\"\"\n", KEYS).expect("parses");
    assert_eq!(s.get("MDM_A"), Some("a=b"));
    assert_eq!(s.get("MDM_C"), Some(""));
    assert_eq!(s.get("MDM_B_2"), None);
}

/// Catches a required key that may be absent or empty, and a defaulted key that takes
/// the default when it is set empty (an empty value must not hide a typo).
#[test]
fn require_and_get_or_refuse_an_empty_value() {
    let s = Settings::parse("MDM_A=x\nMDM_C=\n", KEYS).expect("parses");
    assert_eq!(s.require("MDM_A"), Ok("x"));
    for key in ["MDM_C", "MDM_B_2"] {
        assert_eq!(
            s.require(key),
            Err(SettingsError::Missing(key.to_owned())),
            "{key}"
        );
    }
    assert_eq!(s.get_or("MDM_A", "d"), Ok("x".to_owned()));
    assert_eq!(s.get_or("MDM_B_2", "d"), Ok("d".to_owned()));
    assert_eq!(
        s.get_or("MDM_C", "d"),
        Err(SettingsError::Missing("MDM_C".to_owned()))
    );
    assert_eq!(
        SettingsError::Missing("MDM_C".to_owned()).to_string(),
        "MDM_C is not set (settings file)"
    );
}
