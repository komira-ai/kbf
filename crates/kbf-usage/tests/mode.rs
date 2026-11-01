//! `UsageMode`.

use kbf_usage::UsageMode;

// Catches a default other than off: a farm that sets no flag must learn nothing.
#[test]
fn the_default_is_off() {
    assert_eq!(UsageMode::default(), UsageMode::Off);
}

// Catches a spelling that parses to the wrong mode, or a printed form that does not parse
// back.
#[test]
fn each_spelling_parses_and_prints_back() {
    for (text, mode) in [
        ("off", UsageMode::Off),
        ("record", UsageMode::Record),
        ("on", UsageMode::On),
    ] {
        assert_eq!(text.parse::<UsageMode>().unwrap(), mode);
        assert_eq!(mode.to_string(), text);
        assert_eq!(mode.as_str(), text);
    }
}

// Catches a parser that is lenient (case, spaces, a prefix, the empty text) and an error
// that does not say what was refused.
#[test]
fn anything_else_is_refused_naming_the_text() {
    for text in [
        "",
        "ON",
        "Off",
        " on",
        "on ",
        "record,on",
        "yes",
        "true",
        "1",
    ] {
        let err = text.parse::<UsageMode>().unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&format!("{text:?}")), "{message}");
        assert!(message.contains("off, record or on"), "{message}");
    }
}

// Catches the table of what each mode does: off learns and serves nothing, record learns
// only, on does both.
#[test]
fn modes_say_what_they_learn_and_serve() {
    assert!(!UsageMode::Off.learns() && !UsageMode::Off.serves());
    assert!(UsageMode::Record.learns() && !UsageMode::Record.serves());
    assert!(UsageMode::On.learns() && UsageMode::On.serves());
}
