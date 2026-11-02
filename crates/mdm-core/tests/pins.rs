//! The pinned upstream tools and the pins file parser.

use mdm_core::pins::{BUILT_IN, Packing, Pins, PinsError, Tool};

const SUM: &str = "286f2d7ed5fe6297ef48010ac6d7d921b90fe16e62367caaf18154ad649329cf";

fn line(tool: &str, version: &str, file: &str) -> String {
    format!("{tool} {version} https://example.com/r/v{version}/{file} {SUM}\n")
}

fn all_but(skip: Tool) -> String {
    Tool::ALL
        .into_iter()
        .filter(|t| *t != skip)
        .map(|t| line(t.name(), "1.0", "a.zip"))
        .collect()
}

fn refusal(text: &str) -> String {
    Pins::parse(text)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

/// Catches a built-in pins file that no longer parses (every install would panic) or
/// that lost a tool's members: each tool's archive must name at least one member, the
/// URLs are GitHub release assets, and the file is the one the crate ships.
#[test]
fn the_built_in_pins_parse_and_name_every_tool() {
    let pins = Pins::built_in();
    assert_eq!(pins, Pins::parse(BUILT_IN).expect("valid"));
    let tools: Vec<Tool> = pins.all().iter().map(|p| p.tool).collect();
    assert_eq!(tools, Tool::ALL);
    for pin in pins.all() {
        assert!(!pin.members().is_empty(), "{}", pin.tool);
        assert_eq!(pin.url.host(), "github.com", "{}", pin.tool);
        assert_eq!(pins.get(pin.tool), pin);
    }
    let caddy = pins.get(Tool::Caddy);
    assert_eq!(caddy.packing, Packing::TarGz);
    assert!(caddy.archive().starts_with("caddy_"), "{}", caddy.archive());
    assert_eq!(pins.get(Tool::Nanohub).packing, Packing::Zip);
    assert_eq!(pins.get(Tool::Nanohub).sha256_hex(), SUM);
}

/// Catches a parser that takes a pin moved in part (a new version with the old URL, a
/// hash cut short or upper-cased), an unknown or doubled tool, a missing one, or a URL
/// that is not an https archive. Each row is one defect.
#[test]
fn a_pin_moved_in_part_or_a_bad_line_is_refused() {
    let good = all_but(Tool::Mdmctl);
    let with = |extra: &str| format!("# pins\n\n{good}{extra}");
    let cases = [
        (
            with(&line("mdmctl", "1.0", "a.zip").replace("v1.0", "v0.9")),
            "line 8: https://example.com/r/v0.9/a.zip does not hold /v1.0/",
        ),
        (
            with(&line("mdmctl", "1.0", "a.zip").replace(&SUM[..2], "")),
            "line 8: sha256",
        ),
        (
            with(&line("mdmctl", "1.0", "a.zip").replace("286f", "286F")),
            "line 8: sha256",
        ),
        (
            with(&line("mdmctl", "1.0", "a.zip").replace("286f", "286g")),
            "line 8: sha256",
        ),
        (
            with(&line("mdmctl", "1.x", "a.zip")),
            "line 8: version \"1.x\"",
        ),
        (
            with(&line("mdmctl", "1..0", "a.zip")),
            "line 8: version \"1..0\"",
        ),
        (
            with(&line("mdmctl", "1.0", "a.tar.xz")),
            "does not end in a .zip or .tar.gz",
        ),
        (
            with(&line("mdmctl", "1.0", "a%20b.zip")),
            "does not end in a .zip or .tar.gz",
        ),
        (
            with(&line("mdmctl", "1.0", ".zip")),
            "does not end in a .zip or .tar.gz",
        ),
        (
            with(&line("mdmctl", "1.0", "a.zip").replace("https", "http")),
            "is not an https:// URL",
        ),
        (
            with(&line("mdmtcl", "1.0", "a.zip")),
            "line 8: \"mdmtcl\" is not a pinned tool",
        ),
        (
            with("mdmctl 1.0 https://example.com/v1.0/a.zip\n"),
            "line 8: 3 fields, not 4",
        ),
        (
            with(&line("lego", "1.0", "a.zip")),
            "line 8: lego is already pinned on line 7",
        ),
        (good.clone(), "the pins have no mdmctl"),
    ];
    for (text, want) in cases {
        let got = refusal(&text);
        assert!(got.contains(want), "{got:?} lacks {want:?}");
    }
    let ok = with(&line("mdmctl", "1.0", "b.tar.gz"));
    let pins = Pins::parse(&ok).expect("valid");
    assert_eq!(pins.get(Tool::Mdmctl).packing, Packing::TarGz);
    assert_eq!(
        Pins::parse(&good).expect_err("no mdmctl"),
        PinsError::Missing(Tool::Mdmctl)
    );
}

/// Catches `--only` taking a misspelt tool (nothing would be fetched), a tool whose name
/// and display differ, and a tool's archive members that lose the version they hold.
#[test]
fn tools_are_named_and_their_members_carry_the_version() {
    for tool in Tool::ALL {
        assert_eq!(Tool::named(tool.name()), Ok(tool));
        assert_eq!(tool.to_string(), tool.name());
        assert!(!tool.members("1.2.3").is_empty());
    }
    assert!(Tool::named("nanohubs").is_err());
    let members = Tool::Nanodep.members("0.7.0");
    let installed: Vec<&str> = members.iter().map(|(_, i)| *i).collect();
    assert_eq!(installed, ["depserver", "depsyncer"]);
    assert_eq!(
        members[0].0,
        "nanodep-linux-amd64-v0.7.0/depserver-linux-amd64"
    );
    let all: Vec<&str> = Tool::ALL
        .into_iter()
        .flat_map(|t| t.members("1"))
        .map(|(_, installed)| installed)
        .collect();
    assert_eq!(
        all,
        [
            "nanohub",
            "depserver",
            "depsyncer",
            "scepserver",
            "caddy",
            "lego",
            "mdmctl"
        ]
    );
}
