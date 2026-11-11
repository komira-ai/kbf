//! The apt side: the `Packages` stanza of a package, merging new packages into an existing index
//! (monotonic, bounded), `Packages` / `Packages.gz`, and the `Release` with the checksums apt
//! checks. The text it writes is the text the repository's other publisher writes for the same
//! inputs (the golden test pins it), so two publishers feed one apt repository one format.
//!
//! Layout (suite `stable` or `testing`, component `main`):
//!
//! ```text
//! dists/stable/Release
//! dists/stable/main/binary-<arch>/Packages and Packages.gz
//! pool/kbf/kbf_<version>_<arch>.deb
//! ```
//!
//! `Release` lists the SHA-256 and size of every index file, which is what `apt-get update`
//! verifies after it has verified the signature of `Release` (signing is not done here).
//! Everything is a pure function of its inputs: the `Date` is a parameter, nothing reads a clock.

use std::cmp::Ordering;
use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use super::control::{self, Field};
use super::deb::control_of;
use super::state::{POOL_DIR, Suite, valid_arch};
use crate::channels::Error;
use crate::channels::numbers;

/// Fields the index writes itself, from the file: a control file that carries one would forge the
/// file's name, size or checksums. Field names are not case sensitive in Debian control files.
const RESERVED: [&str; 5] = ["Filename", "Size", "SHA256", "MD5sum", "SHA1"];

/// One package of an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stanza {
    /// The package name.
    pub package: String,
    /// The version as numbers.
    pub version: (u64, u64, u64),
    /// The Debian architecture.
    pub architecture: String,
    /// The SHA-256 of the package file, as lower case hex.
    pub sha256: String,
    /// The stanza text, ending in `\n`.
    pub text: String,
}

/// The lower case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn refused(why: String) -> Error {
    Error::Failed(why)
}

/// A stanza from its fields: the package, version, architecture and checksum must be there.
fn from_fields(text: &str) -> Result<Stanza, Error> {
    let fields = control::parse(text)?;
    let get = |name: &str| {
        control::get(&fields, name)
            .ok_or_else(|| refused(format!("a stanza without {name}: {text:?}")))
    };
    let version = get("Version")?;
    Ok(Stanza {
        package: get("Package")?.to_owned(),
        version: numbers(version)
            .ok_or_else(|| refused(format!("version {version:?} is not X.Y.Z")))?,
        architecture: get("Architecture")?.to_owned(),
        sha256: get("SHA256")?.to_owned(),
        text: text.to_owned(),
    })
}

/// A package name as Debian's policy allows (lower case letters, digits, `+`, `-`, `.`, starting
/// with a letter or digit, two characters at least): it becomes a path in the pool.
fn valid_name(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+-.".contains(&b))
}

/// The index stanza of a package: its control fields, then `Filename` (below the product's pool
/// directory), `Size` and `SHA256` of the file, `Description` last.
///
/// # Errors
/// Bytes that are not a package, a name, version or architecture that cannot be a path in the
/// pool, or a control file that already carries a field the index writes itself.
pub fn stanza_of(deb: &[u8]) -> Result<Stanza, Error> {
    let fields = control::parse(&control_of(deb)?)?;
    let get = |name: &str| {
        control::get(&fields, name)
            .ok_or_else(|| refused(format!("the package's control file has no {name}")))
    };
    let (package, version, architecture) = (get("Package")?, get("Version")?, get("Architecture")?);
    if !valid_name(package) || numbers(version).is_none() || !valid_arch(architecture) {
        return Err(refused(format!(
            "{package}_{version}_{architecture} cannot be a file of the pool"
        )));
    }
    if let Some(f) = fields
        .iter()
        .find(|f| RESERVED.iter().any(|r| f.name.eq_ignore_ascii_case(r)))
    {
        return Err(refused(format!(
            "the package's control file carries {}, which the index writes itself",
            f.name
        )));
    }
    let (description, mut out): (Vec<Field>, Vec<Field>) = fields
        .iter()
        .cloned()
        .partition(|f| f.name == "Description");
    out.push(Field::new(
        "Filename",
        &format!("{POOL_DIR}/{package}_{version}_{architecture}.deb"),
    ));
    out.push(Field::new("Size", &deb.len().to_string()));
    out.push(Field::new("SHA256", &sha256_hex(deb)));
    out.extend(description);
    from_fields(&control::render(&out)?)
}

/// The stanzas of a `Packages` file. An empty text is an empty index.
///
/// # Errors
/// A stanza that is not one.
pub fn parse_packages(text: &str) -> Result<Vec<Stanza>, Error> {
    control::split_stanzas(text)
        .into_iter()
        .map(from_fields)
        .collect()
}

fn same_slot(a: &Stanza, b: &Stanza) -> bool {
    a.package == b.package && a.architecture == b.architecture
}

/// An index after a merge.
#[derive(Debug, PartialEq, Eq)]
pub struct Merged {
    /// Sorted: package, architecture, newest version first. At most `keep` per package and
    /// architecture.
    pub stanzas: Vec<Stanza>,
    /// How many of the added stanzas were new (the others were listed already, byte for byte).
    pub added: usize,
    /// The versions dropped from the index because more than `keep` were listed.
    pub dropped: usize,
}

/// Adds `add` (at most one stanza per package and architecture) to `existing`. A version older
/// than the newest listed for its package and architecture is refused; the same version again is
/// a no-op when the file's sha256 is the same and refused when it is not. Only the newest `keep`
/// versions per package and architecture stay listed.
///
/// # Errors
/// `keep` is 0, a version moves down, or a version is listed with other bytes.
pub fn merge(existing: &[Stanza], add: &[Stanza], keep: usize) -> Result<Merged, Error> {
    if keep == 0 {
        return Err(Error::Usage("--keep is 0; it must be 1 or more".to_owned()));
    }
    let mut all = existing.to_vec();
    let mut added = 0;
    for new in add {
        let listed: Vec<&Stanza> = existing.iter().filter(|e| same_slot(e, new)).collect();
        match listed
            .iter()
            .map(|e| e.version)
            .max()
            .map(|v| new.version.cmp(&v))
        {
            Some(Ordering::Less) => {
                return Err(refused(format!(
                    "refused: {} {}.{}.{} ({}) is older than the newest listed, which would move the channel down",
                    new.package, new.version.0, new.version.1, new.version.2, new.architecture
                )));
            }
            Some(Ordering::Equal) => {
                if listed
                    .iter()
                    .any(|e| e.version == new.version && e.sha256 != new.sha256)
                {
                    return Err(refused(format!(
                        "refused: {} is listed at this version with other bytes",
                        new.package
                    )));
                }
            }
            _ => {
                all.push(new.clone());
                added += 1;
            }
        }
    }
    all.sort_by(|a, b| {
        (&a.package, &a.architecture)
            .cmp(&(&b.package, &b.architecture))
            .then(b.version.cmp(&a.version))
    });
    let mut kept: Vec<Stanza> = Vec::new();
    let (mut run, mut dropped) = (0, 0);
    for (i, stanza) in all.iter().enumerate() {
        run = if i > 0 && same_slot(&all[i - 1], stanza) {
            run + 1
        } else {
            0
        };
        if run < keep {
            kept.push(stanza.clone());
        } else {
            dropped += 1;
        }
    }
    Ok(Merged {
        stanzas: kept,
        added,
        dropped,
    })
}

/// The `Packages` text of one architecture: its stanzas, each followed by a blank line.
pub fn render_packages(stanzas: &[Stanza], arch: &str) -> String {
    stanzas
        .iter()
        .filter(|s| s.architecture == arch)
        .map(|s| format!("{}\n", s.text))
        .collect()
}

/// A file of the index tree: its path below the repository root and its bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// The path below the repository root.
    pub path: String,
    /// The bytes of the file.
    pub data: Vec<u8>,
}

/// What the `Release` says about the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Absent when the `Release` carries no such field (see [`keep_shared_fields`]).
    pub origin: Option<String>,
    /// The `Label` of a first `Release`.
    pub label: Option<String>,
    /// The `Description` of a first `Release`.
    pub description: Option<String>,
    /// Seconds since the epoch: the `Date` field.
    pub date: u64,
}

/// The `Release` of a shared repository belongs to every product that publishes into it: the
/// Origin, Label and Description of the `Release` that exists stay exactly as they are (a field
/// it lacks stays absent), and `info` supplies them only for the first `Release`, when there is
/// none. `existing` is the text of the `Release` that was read.
///
/// # Errors
/// An existing `Release` that is not control text.
pub fn keep_shared_fields(existing: Option<&str>, info: ReleaseInfo) -> Result<ReleaseInfo, Error> {
    let Some(text) = existing.filter(|t| !t.is_empty()) else {
        return Ok(info);
    };
    let fields = control::parse(text)?;
    let field = |name: &str| control::get(&fields, name).map(str::to_owned);
    Ok(ReleaseInfo {
        origin: field("Origin"),
        label: field("Label"),
        description: field("Description"),
        date: info.date,
    })
}

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// RFC 2822 in UTC: `Tue, 20 Oct 2026 12:00:00 UTC`.
pub fn format_date(secs: u64) -> String {
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days);
    let of_day = secs % 86_400;
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} UTC",
        // 1970-01-01 was a Thursday.
        DAYS[usize::try_from((days + 3) % 7).unwrap_or(0)],
        MONTHS[month - 1],
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

/// The year, month (1 to 12) and day of the day number `days` since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, usize, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, usize::try_from(month).unwrap_or(1), day)
}

/// A gzip stream with no file name and the time 0: the same bytes for the same input.
fn gzip(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(bytes)
        .and_then(|()| gz.finish())
        .map_err(Error::spawn("gzip".to_owned()))
}

/// The index files of `archs` (`Packages` and `Packages.gz` under
/// `dists/<suite>/main/binary-<arch>/`) and the `Release` that lists them. A stanza whose
/// architecture is not in `archs` is refused: it would be listed nowhere.
///
/// # Errors
/// An architecture that is not a name or is listed twice, or a stanza for an architecture that is
/// not listed.
pub fn tree(
    suite: Suite,
    info: &ReleaseInfo,
    stanzas: &[Stanza],
    archs: &[String],
) -> Result<Vec<TreeFile>, Error> {
    let mut sorted = archs.to_vec();
    sorted.sort();
    if sorted.iter().any(|a| !valid_arch(a)) || sorted.windows(2).any(|w| w[0] == w[1]) {
        return Err(refused(format!(
            "the architectures {archs:?} are not each a name, once"
        )));
    }
    if let Some(s) = stanzas.iter().find(|s| !sorted.contains(&s.architecture)) {
        return Err(refused(format!(
            "{} is for {}, which the index does not list",
            s.package, s.architecture
        )));
    }
    let mut files = Vec::new();
    let mut sums = String::new();
    for arch in &sorted {
        let packages = render_packages(stanzas, arch).into_bytes();
        let gz = gzip(&packages)?;
        for (rel, data) in [
            (format!("main/binary-{arch}/Packages"), packages),
            (format!("main/binary-{arch}/Packages.gz"), gz),
        ] {
            sums.push_str(&format!(" {} {} {rel}\n", sha256_hex(&data), data.len()));
            files.push(TreeFile {
                path: format!("{}{rel}", suite.dir()),
                data,
            });
        }
    }
    let mut fields = Vec::new();
    let mut optional = |name: &str, value: &Option<String>| {
        if let Some(v) = value {
            fields.push(Field::new(name, v));
        }
    };
    optional("Origin", &info.origin);
    optional("Label", &info.label);
    fields.extend([
        Field::new("Suite", suite.name()),
        Field::new("Codename", suite.name()),
        Field::new("Date", &format_date(info.date)),
        Field::new("Architectures", &sorted.join(" ")),
        Field::new("Components", "main"),
    ]);
    if let Some(v) = &info.description {
        fields.push(Field::new("Description", v));
    }
    // The index files are also served by their hash (`by-hash/SHA256/<hash>` below each
    // binary-<arch> directory, which the publish writes before the plain files), so a client that
    // reads a Release finds the files it lists whatever is written next.
    fields.push(Field::new("Acquire-By-Hash", "yes"));
    let release = format!("{}SHA256:\n{sums}", control::render(&fields)?);
    files.push(TreeFile {
        path: format!("{}Release", suite.dir()),
        data: release.into_bytes(),
    });
    Ok(files)
}

#[cfg(test)]
mod tests;
