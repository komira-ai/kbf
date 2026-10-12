//! The kit's upstream binaries: one pin per tool (version, download URL, sha256 of the
//! download), compiled in from `templates/mdm/pins.txt`, and what each archive holds.
//!
//! The pins file is read strictly, so a pin moved in part is refused rather than
//! installed. Each line is `<tool> <version> <url> <sha256>`, separated by spaces; blank
//! lines and lines starting with `#` are skipped. Refused: a tool not in [`Tool`], a
//! tool pinned twice or not at all, a version that is not dotted numbers, a URL that is
//! not `https://`, does not hold `/v<version>/`, or does not end in a `.zip` or
//! `.tar.gz` file name, and a sha256 that is not 64 lower-case hex digits.

use std::fmt;

use crate::https::HttpsUrl;

/// The pins compiled into this binary.
pub const BUILT_IN: &str = include_str!("../../templates/mdm/pins.txt");

/// A tool the kit installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tool {
    Nanohub,
    Nanodep,
    Scepserver,
    Caddy,
    Lego,
    Mdmctl,
}

impl Tool {
    pub const ALL: [Tool; 6] = [
        Tool::Nanohub,
        Tool::Nanodep,
        Tool::Scepserver,
        Tool::Caddy,
        Tool::Lego,
        Tool::Mdmctl,
    ];

    /// The tool's name in the pins file and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Nanohub => "nanohub",
            Tool::Nanodep => "nanodep",
            Tool::Scepserver => "scepserver",
            Tool::Caddy => "caddy",
            Tool::Lego => "lego",
            Tool::Mdmctl => "mdmctl",
        }
    }

    /// The tool named `name`, for `--only`.
    pub fn named(name: &str) -> Result<Tool, String> {
        Tool::ALL
            .into_iter()
            .find(|t| t.name() == name)
            .ok_or(format!("{name:?} is not a pinned tool"))
    }

    /// The `(archive member, installed name)` pairs of the tool's archive at `version`.
    pub fn members(self, version: &str) -> Vec<(String, &'static str)> {
        let v = version;
        match self {
            Tool::Nanohub => vec![(
                format!("nanohub-linux-amd64-v{v}/nanohub-linux-amd64"),
                "nanohub",
            )],
            Tool::Nanodep => vec![
                (
                    format!("nanodep-linux-amd64-v{v}/depserver-linux-amd64"),
                    "depserver",
                ),
                (
                    format!("nanodep-linux-amd64-v{v}/depsyncer-linux-amd64"),
                    "depsyncer",
                ),
            ],
            Tool::Scepserver => vec![("scepserver-linux-amd64".to_owned(), "scepserver")],
            Tool::Caddy => vec![("caddy".to_owned(), "caddy")],
            Tool::Lego => vec![("lego".to_owned(), "lego")],
            Tool::Mdmctl => vec![("build/linux/mdmctl".to_owned(), "mdmctl")],
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How an archive is packed, from its file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    Zip,
    TarGz,
}

/// One tool's pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub tool: Tool,
    pub version: String,
    pub url: HttpsUrl,
    pub sha256: [u8; 32],
    pub packing: Packing,
}

impl Pin {
    /// The download's file name, the URL's last part.
    pub fn archive(&self) -> &str {
        self.url.file_name()
    }

    pub fn members(&self) -> Vec<(String, &'static str)> {
        self.tool.members(&self.version)
    }

    pub fn sha256_hex(&self) -> String {
        hex::encode(self.sha256)
    }
}

/// Every tool's pin, in [`Tool::ALL`] order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pins(Vec<Pin>);

/// Why a pins file was refused. Line numbers count from 1.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PinsError {
    #[error("pins line {line}: {why}")]
    Line { line: usize, why: String },
    #[error("the pins have no {0}")]
    Missing(Tool),
}

impl Pins {
    pub fn built_in() -> Pins {
        Pins::parse(BUILT_IN).expect("templates/mdm/pins.txt is valid (a test checks it)")
    }

    pub fn parse(text: &str) -> Result<Pins, PinsError> {
        let mut pins: Vec<(Pin, usize)> = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            if raw.trim().is_empty() || raw.starts_with('#') {
                continue;
            }
            let pin = pin(raw).map_err(|why| PinsError::Line { line, why })?;
            if let Some((_, first)) = pins.iter().find(|(p, _)| p.tool == pin.tool) {
                let why = format!("{} is already pinned on line {first}", pin.tool);
                return Err(PinsError::Line { line, why });
            }
            pins.push((pin, line));
        }
        let mut ordered = Vec::new();
        for tool in Tool::ALL {
            let Some(i) = pins.iter().position(|(p, _)| p.tool == tool) else {
                return Err(PinsError::Missing(tool));
            };
            ordered.push(pins.swap_remove(i).0);
        }
        Ok(Pins(ordered))
    }

    pub fn all(&self) -> &[Pin] {
        &self.0
    }

    pub fn get(&self, tool: Tool) -> &Pin {
        // `parse` keeps one pin per tool, in `Tool::ALL` order, which is declaration order.
        &self.0[tool as usize]
    }
}

fn pin(raw: &str) -> Result<Pin, String> {
    let fields: Vec<&str> = raw.split_whitespace().collect();
    let [name, version, url, sum] = fields[..] else {
        return Err(format!(
            "{} fields, not 4 (tool version url sha256)",
            fields.len()
        ));
    };
    let tool = Tool::named(name)?;
    let dotted = version
        .split('.')
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if !dotted {
        return Err(format!("version {version:?} is not dotted numbers"));
    }
    let url = HttpsUrl::new(url).map_err(|e| e.to_string())?;
    if !url.path().contains(&format!("/v{version}/")) {
        return Err(format!(
            "{url} does not hold /v{version}/: move the version and the URL together"
        ));
    }
    let file = url.file_name();
    let plain = !file.starts_with('.')
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    let packing = match file {
        _ if !plain => None,
        f if f.ends_with(".zip") => Some(Packing::Zip),
        f if f.ends_with(".tar.gz") => Some(Packing::TarGz),
        _ => None,
    };
    let Some(packing) = packing else {
        return Err(format!("{url} does not end in a .zip or .tar.gz file name"));
    };
    let mut sha256 = [0u8; 32];
    let lower = !sum.chars().any(|c| c.is_ascii_uppercase());
    if !lower || hex::decode_to_slice(sum, &mut sha256).is_err() {
        return Err(format!("sha256 {sum:?} is not 64 lower-case hex digits"));
    }
    Ok(Pin {
        tool,
        version: version.to_owned(),
        url,
        sha256,
        packing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Catches a built-in pins file that no longer parses (every install would panic)
    /// or that lost a tool's members: each tool's archive must name at least one
    /// member, and the URLs are GitHub release assets.
    #[test]
    fn the_built_in_pins_parse_and_name_every_tool() {
        let pins = Pins::built_in();
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
        assert_eq!(pins.get(Tool::Nanohub).sha256_hex().len(), 64);
    }

    /// Catches a parser that takes a pin moved in part (a new version with the old
    /// URL, a hash cut short or upper-cased), an unknown or doubled tool, a missing
    /// one, or a URL that is not an https archive. Each row is one planted defect.
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
    }

    /// Catches `--only` taking a misspelt tool (nothing would be fetched) and a
    /// tool's archive members that lose the version they hold.
    #[test]
    fn tools_are_named_and_their_members_carry_the_version() {
        for tool in Tool::ALL {
            assert_eq!(Tool::named(tool.name()), Ok(tool));
            assert_eq!(tool.to_string(), tool.name());
        }
        assert!(Tool::named("nanohubs").is_err());
        let members = Tool::Nanodep.members("0.7.0");
        let installed: Vec<&str> = members.iter().map(|(_, i)| *i).collect();
        assert_eq!(installed, ["depserver", "depsyncer"]);
        assert_eq!(
            members[0].0,
            "nanodep-linux-amd64-v0.7.0/depserver-linux-amd64"
        );
    }
}
