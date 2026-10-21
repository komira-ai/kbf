use super::*;

const EXAMPLE: &str = include_str!("../../templates/linux/swap.example");

fn refusal(text: &str) -> String {
    SwapConfig::parse(text)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

/// Catches an unknown key (a typo, or a key of the retired unit settings) or a repeat
/// taken as a setting nobody reads; a character a step cannot carry; a relative swapfile.
#[test]
fn bad_settings_are_refused() {
    let cases = [
        (format!("{EXAMPLE}KBF_SWAP=1\n"), "unknown key KBF_SWAP"),
        (
            format!("{EXAMPLE}KBF_RAM_GIB=1\nKBF_RAM_GIB=2\n"),
            "KBF_RAM_GIB is already set",
        ),
        (
            format!("{EXAMPLE}KBF_SWAP_SIZE=1G\n"),
            "KBF_SWAP_SIZE is already set",
        ),
        (
            "KBF_SWAP_SIZE=1G;2\n".to_owned(),
            "KBF_SWAP_SIZE holds a character the swap step cannot carry: 1G;2",
        ),
        (
            "KBF_SWAP_FILE=swapfile\n".to_owned(),
            "KBF_SWAP_FILE: \"swapfile\" is not absolute",
        ),
    ];
    for (text, want) in &cases {
        let got = refusal(text);
        assert!(got.contains(want), "want {want:?}, got {got:?}");
    }
}

/// Catches a default that differs from what `swap.example` documents (a host configured
/// by leaving keys out would get another swapfile than the example says), and an empty
/// value read as a value rather than defaulted.
#[test]
fn an_unset_or_empty_key_takes_the_default_the_example_shows() {
    let example = SwapConfig::parse(EXAMPLE).expect("the example is valid");
    assert_eq!(example.swap_file.as_str(), "/swapfile");
    assert_eq!(example.swap_size, "128G");
    assert_eq!(SwapConfig::parse("").expect("empty"), example);
    assert_eq!(
        SwapConfig::parse("KBF_SWAP_FILE=\nKBF_SWAP_SIZE=\"\"\n").expect("empty values"),
        example
    );
}

const OLD_NODE_CONF: &str = include_str!("testdata/node.conf");

/// Catches a node.conf written for the retired unit verbs refused by `linux swap` (an
/// operator's existing file must keep working), and its swap keys not read from it.
#[test]
fn an_old_full_node_conf_is_accepted_and_its_swap_keys_are_read() {
    let old = OLD_NODE_CONF.replace("KBF_SWAP_SIZE=128G", "KBF_SWAP_SIZE=64G");
    let c = SwapConfig::parse(&old).expect("accepted");
    assert_eq!(c.swap_file.as_str(), "/swapfile");
    assert_eq!(c.swap_size, "64G");
}

/// Catches a key that was never valid being swallowed with the retired ones.
#[test]
fn a_key_that_was_never_valid_is_still_refused() {
    let text = format!("{OLD_NODE_CONF}KBF_RAM=1\n");
    assert!(
        refusal(&text).contains("unknown key KBF_RAM"),
        "{}",
        refusal(&text)
    );
}

/// Catches the warning naming a swap key (they are read, not retired), and a file with
/// no retired key warning.
#[test]
fn only_retired_keys_are_named_in_the_warning() {
    let c = SwapConfig::parse(OLD_NODE_CONF).expect("accepted");
    assert!(c.retired.contains(&"KBF_HOST_NAME") && !c.retired.contains(&"KBF_SWAP_FILE"));
    assert_eq!(c.retired.len(), 32);
    assert!(
        SwapConfig::parse(EXAMPLE)
            .expect("ok")
            .retired_warning()
            .is_none()
    );
}
