//! Key families and their digests: what the copy is checked by.
//!
//! A *family* is the first path segment of a key with its slash (`op/`, `lease/`, `meta/`);
//! a key with no slash is a family of its own. Each family has a row count and a SHA-256
//! digest of its rows in key order (bytewise, as both stores sort keys):
//!
//! - a key/value row contributes its key, its value, its version and its expiry. The key
//!   and the value are each an 8-byte big-endian length followed by the bytes; the version
//!   is 8 bytes big endian; the expiry is one byte (0 for none, 1 for one) followed by 8
//!   bytes big endian (0 for none). The expiry is in **microseconds** on the Unix clock,
//!   the unit the Postgres store keeps: the state file keeps nanoseconds, and a row read
//!   from it is converted (rounded down) before it is hashed;
//! - a counter contributes its name (a length and the bytes) and its value (8 bytes, big
//!   endian).
//!
//! Key/value rows and counters are two separate sets of families. The same code hashes the
//! rows read from the state file and the rows read back from Postgres, so a digest differs
//! between the two only when a key, a value, a version, an expiry or a row differs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// A key/value row, in the units of the Postgres store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The key.
    pub key: String,
    /// The value.
    pub value: Vec<u8>,
    /// The revision that wrote the value.
    pub version: i64,
    /// When the row expires: microseconds on the Unix clock.
    pub expires_us: Option<i64>,
}

/// A counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counter {
    /// The counter's name.
    pub name: String,
    /// Its value.
    pub value: i64,
}

/// One family: its name, how many rows it holds, and the digest of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Family {
    /// The key up to and including its first slash, or the whole key.
    pub name: String,
    /// The rows in the family.
    pub row_count: u64,
    /// The digest, in lower-case hex.
    pub digest: String,
}

/// Every family of a store: those of its key/value rows, and those of its counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// The key/value families, by name.
    pub keys: Vec<Family>,
    /// The counter families, by name.
    pub counters: Vec<Family>,
}

/// The family of `key`: its first path segment with the slash, or the whole key when it
/// has no slash.
#[must_use]
pub fn family_of(key: &str) -> &str {
    key.split_inclusive('/').next().unwrap_or(key)
}

/// The rows of one set of families, hashed as they are added.
#[derive(Default)]
struct Families(BTreeMap<String, (Sha256, u64)>);

impl Families {
    fn add(&mut self, name: &str, record: &[u8]) {
        let (hash, count) = self.0.entry(name.to_owned()).or_default();
        hash.update(record);
        *count += 1;
    }

    fn finish(self) -> Vec<Family> {
        let mut families = Vec::new();
        for (name, (hash, row_count)) in self.0 {
            let digest = hex::encode(hash.finalize());
            families.push(Family {
                name,
                row_count,
                digest,
            });
        }
        families
    }
}

/// Appends `bytes` to `buf` behind an 8-byte big-endian length.
fn put(buf: &mut Vec<u8>, bytes: &[u8]) {
    let length = bytes.len() as u64;
    buf.extend_from_slice(&length.to_be_bytes());
    buf.extend_from_slice(bytes);
}

fn encode_entry(entry: &Entry) -> Vec<u8> {
    let mut buf = Vec::new();
    put(&mut buf, entry.key.as_bytes());
    put(&mut buf, &entry.value);
    buf.extend_from_slice(&entry.version.to_be_bytes());
    buf.push(u8::from(entry.expires_us.is_some()));
    buf.extend_from_slice(&entry.expires_us.unwrap_or_default().to_be_bytes());
    buf
}

fn encode_counter(counter: &Counter) -> Vec<u8> {
    let mut buf = Vec::new();
    put(&mut buf, counter.name.as_bytes());
    buf.extend_from_slice(&counter.value.to_be_bytes());
    buf
}

/// The families of `entries` and `counters`, whatever order they are given in.
#[must_use]
pub fn summarize(entries: &[Entry], counters: &[Counter]) -> Summary {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.key.as_bytes().cmp(b.key.as_bytes()));
    let mut keys = Families::default();
    for entry in sorted {
        keys.add(family_of(&entry.key), &encode_entry(entry));
    }
    let mut named: Vec<&Counter> = counters.iter().collect();
    named.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut names = Families::default();
    for counter in named {
        names.add(family_of(&counter.name), &encode_counter(counter));
    }
    Summary {
        keys: keys.finish(),
        counters: names.finish(),
    }
}

impl Summary {
    /// The key/value rows in all families.
    #[must_use]
    pub fn key_rows(&self) -> u64 {
        self.keys.iter().map(|family| family.row_count).sum()
    }

    /// The counters in all families.
    #[must_use]
    pub fn counter_rows(&self) -> u64 {
        self.counters.iter().map(|family| family.row_count).sum()
    }

    /// The families as text: a table of name, row count and digest for each set.
    #[must_use]
    pub fn table(&self) -> String {
        let mut text = String::new();
        add_table(&mut text, "key families", &self.keys);
        add_table(&mut text, "counter families", &self.counters);
        text
    }

    /// How `copy` differs from this summary, one line per family that does not match.
    /// Empty when they are the same.
    #[must_use]
    pub fn differences(&self, copy: &Summary) -> Vec<String> {
        let mut out = Vec::new();
        compare("key family", &self.keys, &copy.keys, &mut out);
        compare("counter family", &self.counters, &copy.counters, &mut out);
        out
    }
}

fn add_table(text: &mut String, title: &str, families: &[Family]) {
    let _ = writeln!(text, "{title} (rows, digest):");
    let width = families.iter().map(|family| family.name.len()).max();
    let width = width.unwrap_or(0);
    for family in families {
        let name = &family.name;
        let rows = family.row_count;
        let digest = &family.digest;
        let _ = writeln!(text, "  {name:<width$}  {rows:>6}  {digest}");
    }
}

fn compare(kind: &str, want: &[Family], got: &[Family], out: &mut Vec<String>) {
    let mut names: BTreeSet<&str> = want.iter().map(|f| f.name.as_str()).collect();
    names.extend(got.iter().map(|f| f.name.as_str()));
    for name in names {
        let left = want.iter().find(|f| f.name == name);
        let right = got.iter().find(|f| f.name == name);
        if left != right {
            let (state, copy) = (show(left), show(right));
            out.push(format!("{kind} {name}: state file {state}, copy {copy}"));
        }
    }
}

fn show(family: Option<&Family>) -> String {
    match family {
        Some(f) => format!("{} rows, digest {}", f.row_count, f.digest),
        None => "absent".to_owned(),
    }
}
