//! The per-binary entitlement allow list ([`ALLOWLIST`]; docs/design/macos-vm-guests.md
//! section 7): the entitlements `sign-darwin` signs each macOS binary with, and the
//! comparison `sign-darwin` and `darwin-asset` make of a signature against it.
//!
//! The list has one line per binary that gets any entitlement: its file name, then its
//! entitlements, separated by blanks; `#` starts a comment. Each entitlement is a
//! boolean set to true. A binary the list does not name gets none.
//!
//! [`AllowList::read`] reads the whole file and fails closed: a missing or unreadable
//! file, a malformed line, a binary listed twice or an entitlement repeated on its line
//! is an error, whatever binary is asked about. So a damaged list can never read as
//! "no entitlements" for an unlisted binary.
//!
//! [`compare`] is the verifier. For an unlisted binary the signature must carry no
//! entitlements at all. For a listed one, the signature's entitlements, as `codesign
//! --display --entitlements - --xml` prints them, must be a plist dictionary of exactly
//! the listed keys, each `<true/>`: one more, one fewer, another key or another value
//! fails. The reader of that plist ([`signed_keys`]) accepts only that shape, on one
//! line (as `codesign` on macOS 15 prints it) or on many, and refuses everything else,
//! so it cannot accept what it does not understand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The allow list, relative to the repository root.
pub const ALLOWLIST: &str = "tools/ci/darwin-entitlements.txt";

/// What is wrong with one line of the list.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Defect {
    #[error("'{0}' is not a binary name")]
    NotABinaryName(String),
    #[error("{0} lists no entitlement (leave a binary that gets none out)")]
    NoEntitlement(String),
    #[error("{0} is listed twice")]
    ListedTwice(String),
    #[error("'{0}' is not an entitlement name")]
    NotAnEntitlement(String),
    #[error("{0} repeats an entitlement")]
    Repeats(String),
}

/// Why the list could not be read, or a signature does not match it.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the entitlement allow list {} is missing", .0.display())]
    Missing(PathBuf),
    #[error("the entitlement allow list {} cannot be read: {why}", .path.display())]
    Unreadable { path: PathBuf, why: String },
    #[error("{}:{line}: {defect}", .path.display())]
    Malformed {
        path: PathBuf,
        line: usize,
        defect: Defect,
    },
    #[error("the allow list gives {name} no entitlements, but its signature carries: {signed}")]
    UnlistedCarries { name: String, signed: String },
    #[error(
        "the allow list gives {name} the entitlements {}, but its signature carries none",
        .want.join(" ")
    )]
    ListedCarriesNone { name: String, want: Vec<String> },
    #[error(
        "the entitlements in the signature of {name} are not a plist dictionary of true \
         values: {signed}"
    )]
    NotTrueValues { name: String, signed: String },
    #[error(
        "the signature of {name} carries other entitlements than the allow list gives it: \
         want {}; signed {}",
        .want.join(" "),
        .signed.join(" ")
    )]
    Other {
        name: String,
        want: Vec<String>,
        signed: Vec<String>,
    },
}

/// The parsed list: each listed binary's entitlements, sorted.
#[derive(Debug, PartialEq, Eq)]
pub struct AllowList {
    binaries: BTreeMap<String, Vec<String>>,
}

/// A binary name or an entitlement: ASCII letters, digits, `.`, `_` and `-`, not
/// starting with a punctuation mark. So nothing needs escaping in the plist.
pub fn is_word(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl AllowList {
    /// Reads and parses the list at `path`.
    ///
    /// # Errors
    /// The file is missing or unreadable, or [`AllowList::parse`] refuses it.
    pub fn read(path: &Path) -> Result<AllowList, Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => AllowList::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::Missing(path.to_owned()))
            }
            Err(e) => Err(Error::Unreadable {
                path: path.to_owned(),
                why: e.to_string(),
            }),
        }
    }

    /// Parses the text of the list read from `path` (named in the errors).
    ///
    /// # Errors
    /// The first malformed line, by its 1-based number.
    pub fn parse(text: &str, path: &Path) -> Result<AllowList, Error> {
        let mut binaries = BTreeMap::new();
        for (n, line) in text.lines().enumerate() {
            let malformed = |defect| Error::Malformed {
                path: path.to_owned(),
                line: n + 1,
                defect,
            };
            let content = line.split('#').next().unwrap_or_default();
            let mut words = content.split_whitespace();
            let Some(name) = words.next() else {
                continue;
            };
            if !is_word(name) {
                return Err(malformed(Defect::NotABinaryName(name.to_owned())));
            }
            let mut keys: Vec<String> = words.map(str::to_owned).collect();
            if keys.is_empty() {
                return Err(malformed(Defect::NoEntitlement(name.to_owned())));
            }
            if binaries.contains_key(name) {
                return Err(malformed(Defect::ListedTwice(name.to_owned())));
            }
            if let Some(k) = keys.iter().find(|k| !is_word(k)) {
                return Err(malformed(Defect::NotAnEntitlement(k.clone())));
            }
            keys.sort();
            let count = keys.len();
            keys.dedup();
            if keys.len() != count {
                return Err(malformed(Defect::Repeats(name.to_owned())));
            }
            binaries.insert(name.to_owned(), keys);
        }
        Ok(AllowList { binaries })
    }

    /// The entitlements the list gives the binary named `name`, sorted; none when the
    /// list does not name it.
    pub fn entitlements(&self, name: &str) -> &[String] {
        self.binaries.get(name).map_or(&[], Vec::as_slice)
    }

    /// The entitlements plist to sign the binary named `name` with ([`render`]), or
    /// `None` when the list gives it none.
    pub fn plist(&self, name: &str) -> Option<String> {
        let keys = self.entitlements(name);
        (!keys.is_empty()).then(|| render(keys))
    }
}

/// The plist giving each of `keys` the value true, in the order given.
pub fn render(keys: &[String]) -> String {
    let mut plist = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for k in keys {
        plist.push_str(&format!("\t<key>{k}</key>\n\t<true/>\n"));
    }
    plist.push_str("</dict>\n</plist>\n");
    plist
}

/// The keys of the entitlements plist `xml`, sorted, when it is a dictionary whose
/// every key is a [word](is_word) set to `<true/>`, each key once; `None` for anything
/// else. An XML declaration and a doctype may come first; blanks between tags are
/// ignored.
pub fn signed_keys(xml: &str) -> Option<Vec<String>> {
    let mut rest = xml.trim_start();
    if rest.starts_with("<?xml") {
        rest = rest[rest.find("?>")? + 2..].trim_start();
    }
    if rest.starts_with("<!DOCTYPE") {
        rest = rest[rest.find('>')? + 1..].trim_start();
    }
    let tag = |rest: &mut &str, t: &str| -> Option<()> {
        *rest = rest.strip_prefix(t)?.trim_start();
        Some(())
    };
    tag(&mut rest, "<plist version=\"1.0\">")?;
    let mut keys = Vec::new();
    if tag(&mut rest, "<dict/>").is_none() {
        tag(&mut rest, "<dict>")?;
        while tag(&mut rest, "</dict>").is_none() {
            tag(&mut rest, "<key>")?;
            let end = rest.find("</key>")?;
            let key = &rest[..end];
            if !is_word(key) {
                return None;
            }
            keys.push(key.to_owned());
            rest = rest[end + "</key>".len()..].trim_start();
            tag(&mut rest, "<true/>")?;
        }
    }
    tag(&mut rest, "</plist>")?;
    if !rest.is_empty() {
        return None;
    }
    keys.sort();
    let count = keys.len();
    keys.dedup();
    (keys.len() == count).then_some(keys)
}

/// Whether `signed`, the entitlements the signature of the binary named `name` carries
/// (as `codesign --display --entitlements - --xml` prints them; empty for none), are
/// exactly those `list` gives it: the line to print when they are.
///
/// # Errors
/// They are not, with what each side holds.
pub fn compare(list: &AllowList, name: &str, signed: &str) -> Result<String, Error> {
    let want = list.entitlements(name);
    let signed = signed.trim();
    match (want.is_empty(), signed.is_empty()) {
        (true, true) => Ok(format!(
            "entitlements of {name}: none, as the allow list says"
        )),
        (true, false) => Err(Error::UnlistedCarries {
            name: name.to_owned(),
            signed: signed.to_owned(),
        }),
        (false, true) => Err(Error::ListedCarriesNone {
            name: name.to_owned(),
            want: want.to_vec(),
        }),
        (false, false) => match signed_keys(signed) {
            None => Err(Error::NotTrueValues {
                name: name.to_owned(),
                signed: signed.to_owned(),
            }),
            Some(keys) if keys == want => Ok(format!(
                "entitlements of {name}: exactly {}, as the allow list says",
                want.join(" ")
            )),
            Some(keys) => Err(Error::Other {
                name: name.to_owned(),
                want: want.to_vec(),
                signed: keys,
            }),
        },
    }
}
