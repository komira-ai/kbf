//! The kit's upstream binaries: one pin per tool (version, download URL, sha256 of the
//! download), compiled in from `pins.txt`, and what each archive holds.
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
pub const BUILT_IN: &str = include_str!("../pins.txt");

/// A tool the kit installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tool {
    /// NanoHUB: the MDM server.
    Nanohub,
    /// NanoDEP: the Apple Business Manager side.
    Nanodep,
    /// The SCEP server.
    Scepserver,
    /// Caddy: the HTTPS proxy.
    Caddy,
    /// lego: the ACME client.
    Lego,
    /// mdmctl: MicroMDM's command line.
    Mdmctl,
}

impl Tool {
    /// Every tool, in the order of the pins.
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
    /// A `.zip`.
    Zip,
    /// A `.tar.gz`.
    TarGz,
}

/// One tool's pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    /// The tool.
    pub tool: Tool,
    /// Its version, dotted numbers.
    pub version: String,
    /// Where the archive is downloaded from.
    pub url: HttpsUrl,
    /// The sha256 of the download.
    pub sha256: [u8; 32],
    /// How the archive is packed.
    pub packing: Packing,
}

impl Pin {
    /// The download's file name, the URL's last part.
    pub fn archive(&self) -> &str {
        self.url.file_name()
    }

    /// The `(archive member, installed name)` pairs of the pinned archive.
    pub fn members(&self) -> Vec<(String, &'static str)> {
        self.tool.members(&self.version)
    }

    /// The sha256 as 64 lower-case hex digits.
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
    /// A line that is not a valid pin.
    #[error("pins line {line}: {why}")]
    Line {
        /// The line number.
        line: usize,
        /// What is wrong with it.
        why: String,
    },
    /// A tool with no pin.
    #[error("the pins have no {0}")]
    Missing(Tool),
}

impl Pins {
    /// The pins compiled into this crate.
    pub fn built_in() -> Pins {
        Pins::parse(BUILT_IN).expect("pins.txt is valid (a test checks it)")
    }

    /// The pins in `text`, one per tool, or why the text is refused.
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

    /// Every pin, in [`Tool::ALL`] order.
    pub fn all(&self) -> &[Pin] {
        &self.0
    }

    /// The pin of `tool`.
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
