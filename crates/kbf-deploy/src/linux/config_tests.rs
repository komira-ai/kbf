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
            format!("{EXAMPLE}KBF_RAM_GIB=256\n"),
            "unknown key KBF_RAM_GIB",
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
